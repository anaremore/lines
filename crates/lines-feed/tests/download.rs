//! Download past days (decision 0030 §8): the engine and the store write path, driven by the
//! synthetic demo source (no network, no vendor).

use std::sync::Arc;
use std::time::Duration;

use lines_core::{BarSeries, CivilDate, ContractKey, Month, Product, Stitcher, UtcMillis};
use lines_feed::catalog;
use lines_feed::disk::day::{DayState, DayWriter};
use lines_feed::disk::segment::{End, SegmentReader};
use lines_feed::disk::{ContractDir, ContractWriter, StoreConfig};
use lines_feed::download::{
    self, Cancel, Charge, DayRequest, Outcome, PastDays, Target, Update, demo::Demo,
};
use lines_feed::native::{self, NativeEvent};
use lines_feed::store::ChartStore;
use lines_feed::{FeedConfig, Source, new_store, run_feed};

const SOURCE: &str = "demo";

fn root(name: &str) -> lines_core::scratch::Scratch {
    lines_core::scratch::Scratch::new(&format!("download-{name}"))
}

fn es() -> Product {
    catalog::get(catalog::ES).unwrap()
}

fn esz6() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn target(root: &std::path::Path) -> Target {
    Target {
        root: root.to_path_buf(),
        source: SOURCE.into(),
        product: es(),
        contract: esz6(),
    }
}

fn day(d: u8) -> CivilDate {
    CivilDate::new(2026, 11, d).unwrap()
}

/// After every test date's session.
fn now() -> UtcMillis {
    UtcMillis::parse_iso("2026-11-09T12:00:00Z").unwrap()
}

/// Run a job to the end; its updates in order.
fn run(job: download::Job) -> Vec<Update> {
    let mut out = Vec::new();
    loop {
        let u = job.next(Duration::from_secs(60)).expect("job stalled");
        let end = u == Update::Finished;
        out.push(u);
        if end {
            break;
        }
    }
    job.join();
    out
}

fn outcomes(updates: &[Update]) -> Vec<(CivilDate, Outcome)> {
    updates
        .iter()
        .filter_map(|u| match u {
            Update::Day { date, outcome } => Some((*date, outcome.clone())),
            _ => None,
        })
        .collect()
}

fn get(demo: Demo, t: &Target, dates: &[(CivilDate, Charge)]) -> Vec<(CivilDate, Outcome)> {
    outcomes(&run(download::download(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        dates.to_vec(),
        now(),
        Cancel::new(),
    )))
}

fn request(date: CivilDate) -> DayRequest {
    let (open, close) = lines_feed::disk::day::session_span(&es(), date).unwrap();
    DayRequest {
        product: es(),
        contract: esz6(),
        date,
        open,
        close,
        contents: download::demo::TOB.id,
    }
}

fn nothing_left(dir: &ContractDir, date: CivilDate) {
    assert!(!dir.segment_path(date).exists(), "no segment");
    let names: Vec<String> = std::fs::read_dir(&dir.path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|n| !n.starts_with(&date.to_string())),
        "no part file or scratch: {names:?}"
    );
    let (open, close) = lines_feed::disk::day::session_span(&es(), date).unwrap();
    let cov = dir.coverage();
    assert!(
        cov.trades.holes(open.0, close.0 - 1) == vec![(open.0, close.0 - 1)],
        "no coverage claimed"
    );
}

#[test]
fn estimate_excludes_stored_days() {
    let root = root("estimate");
    let t = target(&root);
    // Wednesday is stored already.
    assert!(matches!(
        get(Demo::default(), &t, &[(day(4), Charge::Included)])[0].1,
        Outcome::Stored(_)
    ));
    let demo = Demo {
        included_from: Some(day(4)),
        price_usd_micros: 1_630_000,
        ..Demo::default()
    };
    let asked = demo.asked.clone();
    // Tue..Sun: Sat and Sun have no session; Wed is stored.
    let dates = download::dates(day(3), day(8)).unwrap();
    let plan = download::plan(&t, &dates, now());
    assert_eq!(plan[1], (day(4), DayState::Stored { complete: true }));
    assert_eq!(plan[4], (day(7), DayState::NoSession));
    let updates = run(download::estimate(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        dates,
        now(),
        Cancel::new(),
    ));
    let asked_days = asked.lock().unwrap().clone();
    assert_eq!(
        asked_days,
        vec![day(3), day(5), day(6)],
        "never the stored day"
    );
    let charges: Vec<(CivilDate, Charge)> = updates
        .iter()
        .filter_map(|u| match u {
            Update::Estimate { date, result } => Some((*date, result.as_ref().unwrap().charge)),
            _ => None,
        })
        .collect();
    assert_eq!(
        charges,
        vec![
            (
                day(3),
                Charge::Estimated {
                    usd_micros: 1_630_000
                }
            ),
            (day(5), Charge::Included),
            (day(6), Charge::Included),
        ]
    );
    // Downloading it again is refused without asking the provider.
    let demo = Demo::default();
    let asked = demo.asked.clone();
    assert_eq!(
        get(demo, &t, &[(day(4), Charge::Included)])[0].1,
        Outcome::Skipped(DayState::Stored { complete: true })
    );
    assert!(asked.lock().unwrap().is_empty());
}

/// The stored day holds exactly what the source delivered, and the full feed replays it to
/// the same bars as building them from the delivered trades; bars and coverage are claimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_downloaded_day_replays_identically() {
    let root = root("replay");
    let t = target(&root);
    let date = day(5);
    // A bar index exists (a feed ran before): the day's bars merge into it and are claimed.
    std::fs::create_dir_all(&t.dir().path).unwrap();
    lines_feed::disk::bars::save(&t.dir().bars_path(), 60_000, &[]).unwrap();
    let out = get(Demo::default(), &t, &[(date, Charge::Included)]);
    let Outcome::Stored(w) = &out[0].1 else {
        panic!("{out:?}")
    };
    let delivered = Demo::frames(&request(date));
    let (open, close) = lines_feed::disk::day::session_span(&es(), date).unwrap();
    let in_day = |e: &NativeEvent| e.time_us() >= open.0 * 1000 && e.time_us() < close.0 * 1000;
    let expected: Vec<NativeEvent> = delivered
        .iter()
        .flat_map(|(_, ev)| ev.iter().copied().filter(in_day))
        .collect();
    assert_eq!(w.outside, 0);
    assert_eq!(w.trades as usize, delivered.len());

    // Block by block.
    let dir = t.dir();
    let mut r = SegmentReader::open(&dir.segment_path(date)).unwrap();
    assert_eq!(r.header.source, SOURCE);
    assert_eq!(r.header.date, date);
    assert_eq!(r.header.contract, Some(esz6()));
    let mut got = Vec::new();
    loop {
        match r.next_block().unwrap() {
            Ok(b) => native::decode(&b.events, &mut got).unwrap(),
            Err(e) => {
                assert_eq!(e, End::Eof);
                break;
            }
        }
    }
    assert_eq!(got, expected);

    // Coverage and the stored day's state.
    let cov = dir.coverage();
    assert!(cov.trades.holes(open.0, close.0 - 1).is_empty());
    assert!(cov.bars.holes(open.0, close.0 - 1).is_empty());
    assert_eq!(dir.trade_coverage().ranges(), &[(open.0, close.0 - 1)]);

    // The full feed replays the stored day (the contract directory, as `--replay` does) to
    // the bars the delivered trades build.
    let mut want = BarSeries::new(60_000, 100_000);
    let mut st = Stitcher::new(60_000, 0);
    for e in &expected {
        if let NativeEvent::Trade {
            time_us,
            price,
            qty,
            ..
        } = *e
        {
            st.trade(&mut want, UtcMillis(time_us.div_euclid(1000)), price, qty);
        }
    }
    let mut config = FeedConfig::new(Source::Replay {
        path: dir.path.clone(),
        speed: 0.0,
        stop_at: None,
    });
    config.max_bars = 100_000;
    let store = new_store(&config).unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    tokio::time::timeout(Duration::from_secs(60), run_feed(config, store.clone(), rx))
        .await
        .unwrap();
    assert_eq!(store.read().series.bars(), want.bars());
    // Rebuilding from segments (what a lost bar index does) gives the same bars.
    assert_eq!(dir.rebuild_bars(60_000, 100_000).unwrap(), want.bars());
}

#[test]
fn cancel_mid_day_leaves_nothing() {
    let root = root("cancel");
    let t = target(&root);
    let cancel = Cancel::new();
    let demo = Demo {
        cancel_after: Some((500, cancel.clone())),
        ..Demo::default()
    };
    let updates = run(download::download(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        vec![(day(5), Charge::Included), (day(6), Charge::Included)],
        now(),
        cancel,
    ));
    assert_eq!(
        outcomes(&updates),
        vec![(day(5), Outcome::Canceled), (day(6), Outcome::Canceled)]
    );
    nothing_left(&t.dir(), day(5));
    nothing_left(&t.dir(), day(6));
    assert_eq!(t.dir().dates(), vec![]);
    // The day can be downloaded afterwards.
    assert!(matches!(
        get(Demo::default(), &t, &[(day(5), Charge::Included)])[0].1,
        Outcome::Stored(_)
    ));
}

#[test]
fn a_refusal_carries_the_providers_message() {
    let root = root("refuse");
    let t = target(&root);
    let msg = "Your plan doesn't include this dataset before 2025-11-10.";
    let demo = Demo {
        refuse: vec![(day(5), msg.into())],
        ..Demo::default()
    };
    let updates = run(download::estimate(
        Arc::new(demo.clone()),
        t.clone(),
        download::demo::TOB.id,
        vec![day(5)],
        now(),
        Cancel::new(),
    ));
    assert_eq!(
        updates[1],
        Update::Estimate {
            date: day(5),
            result: Err(download::DayError::Refused(download::Refusal(msg.into())))
        }
    );
    assert_eq!(
        get(demo, &t, &[(day(5), Charge::Included)])[0].1,
        Outcome::Refused(msg.into())
    );
    nothing_left(&t.dir(), day(5));
}

#[test]
fn never_more_than_the_accepted_price() {
    let root = root("price");
    let t = target(&root);
    let demo = Demo {
        included_from: Some(day(20)),
        price_usd_micros: 520_000,
        ..Demo::default()
    };
    assert_eq!(
        get(demo.clone(), &t, &[(day(5), Charge::Included)])[0].1,
        Outcome::PriceChanged(Charge::Estimated {
            usd_micros: 520_000
        })
    );
    nothing_left(&t.dir(), day(5));
    let accepted = Charge::Estimated {
        usd_micros: 520_000,
    };
    assert!(matches!(
        get(demo, &t, &[(day(5), accepted)])[0].1,
        Outcome::Stored(_)
    ));
}

/// Decided rule: a contract a live feed is storing is refused (its writer holds the coverage
/// and bar index in memory); a day being downloaded holds the lock like a live writer.
#[test]
fn a_live_writer_and_a_download_never_share_a_contract() {
    let root = root("live");
    let t = target(&root);
    let mut cfg = StoreConfig::new(root.to_path_buf(), SOURCE);
    cfg.flush = Default::default();
    let store = Arc::new(ChartStore::new(
        es(),
        Some(esz6()),
        lines_core::Capabilities::default(),
        SOURCE,
        60_000,
        1000,
    ));
    let (live, _) = ContractWriter::open(&cfg, &es(), Some(esz6()), store.clone()).unwrap();
    let demo = Demo::default();
    let asked = demo.asked.clone();
    assert_eq!(
        get(demo, &t, &[(day(5), Charge::Included)])[0].1,
        Outcome::Busy
    );
    assert!(asked.lock().unwrap().is_empty(), "nothing asked or fetched");
    // The estimate says so up front.
    assert_eq!(download::plan(&t, &[day(5)], now())[0].1, DayState::Busy);
    nothing_left(&t.dir(), day(5));
    drop(live);

    // The other way round: a day being written keeps a live writer out.
    let w = DayWriter::begin(&root, SOURCE, &es(), Some(esz6()), day(5), now()).unwrap();
    let e = ContractWriter::open(&cfg, &es(), Some(esz6()), store.clone())
        .err()
        .expect("locked");
    assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
    drop(w);
    nothing_left(&t.dir(), day(5));
    // Today's session isn't over: never downloaded.
    let during = UtcMillis::parse_iso("2026-11-05T15:00:00Z").unwrap();
    assert_eq!(
        download::plan(&t, &[day(5)], during)[0].1,
        DayState::NotFinished
    );
}

/// Decision 0030 §10: an extension built for another download interface is refused.
#[test]
fn another_interface_version_is_refused() {
    struct Old;
    impl PastDays for Old {
        fn api_version(&self) -> u32 {
            download::API_VERSION + 1
        }
        fn contents(&self, _: &Product) -> Vec<download::Contents> {
            vec![]
        }
        fn estimate(&self, _: &DayRequest) -> Result<download::DayEstimate, download::DayError> {
            unreachable!()
        }
        fn download(
            &self,
            _: &DayRequest,
            _: &mut dyn download::DaySink,
        ) -> Result<(), download::DayError> {
            unreachable!()
        }
    }
    struct Ext;
    impl lines_feed::extension::SourceExtension for Ext {
        fn api_version(&self) -> u32 {
            lines_feed::extension::API_VERSION
        }
        fn name(&self) -> &'static str {
            "old-days"
        }
        fn help(&self) -> &'static str {
            "test"
        }
        fn provider(&self) -> Arc<dyn lines_feed::provider::Provider> {
            Arc::new(lines_feed::native::NativeProvider)
        }
        fn default_product(&self) -> lines_core::ProductId {
            catalog::ES
        }
        fn connect(
            &self,
            _: &Product,
            _: Option<ContractKey>,
        ) -> lines_feed::extension::BoxFuture<
            'static,
            anyhow::Result<Box<dyn lines_feed::extension::DynTransport>>,
        > {
            Box::pin(async { anyhow::bail!("test") })
        }
        fn past_days(&self) -> Option<Arc<dyn PastDays>> {
            Some(Arc::new(Old))
        }
    }
    let e = lines_feed::extension::register(Arc::new(Ext)).unwrap_err();
    assert!(e.to_string().contains("download interface"), "{e}");
    assert!(lines_feed::extension::by_name("old-days").is_none());
}

/// Decision 0030 §8 (fetched once): retention never evicts a downloaded day, though it counts.
#[test]
fn retention_never_evicts_a_downloaded_day() {
    let root = root("retention");
    let t = target(&root);
    assert!(matches!(
        get(Demo::default(), &t, &[(day(3), Charge::Included)])[0].1,
        Outcome::Stored(_)
    ));
    // A newer day stored live (not pinned), in another contract's directory.
    let other = Target {
        contract: ContractKey::monthly(catalog::ES, 2027, Month::new(3).unwrap()),
        ..target(&root)
    };
    let mut w =
        DayWriter::begin(&root, SOURCE, &es(), Some(other.contract), day(4), now()).unwrap();
    let at = w.span().0.0 * 1000 + 1_000_000;
    w.block(at, &Demo::frames(&request(day(4)))[0].1).unwrap();
    let live = w.commit().unwrap();
    // The live-like day: drop its pin, as the live writer never writes one.
    std::fs::remove_file(other.dir().path.join(lines_feed::disk::day::PINNED)).unwrap();
    let evicted = lines_feed::disk::retention::enforce(&root, 0);
    assert_eq!(
        evicted.files,
        vec![live.path.clone()],
        "only the unpinned day goes"
    );
    assert!(t.dir().segment_path(day(3)).exists());
    assert!(evicted.kept > 0, "the pinned day still counts");
}

/// A job whose thread dies still ends (the dialog never hangs).
#[test]
fn a_job_that_panics_still_finishes() {
    struct Boom;
    impl PastDays for Boom {
        fn api_version(&self) -> u32 {
            download::API_VERSION
        }
        fn contents(&self, _: &Product) -> Vec<download::Contents> {
            vec![]
        }
        fn estimate(&self, _: &DayRequest) -> Result<download::DayEstimate, download::DayError> {
            panic!("boom")
        }
        fn download(
            &self,
            _: &DayRequest,
            _: &mut dyn download::DaySink,
        ) -> Result<(), download::DayError> {
            panic!("boom")
        }
    }
    let root = root("panic");
    let updates = run(download::estimate(
        Arc::new(Boom),
        target(&root),
        "x",
        vec![day(5)],
        now(),
        Cancel::new(),
    ));
    assert!(matches!(updates[0], Update::Plan { .. }));
    assert_eq!(updates.last(), Some(&Update::Finished));
}

/// Closing the window drops the job, which is "Cancel remaining" (decided in round 3): the
/// day in flight, maybe already billed, finishes and is stored; no later day is asked for.
#[test]
fn dropping_a_job_finishes_the_day_in_flight() {
    let root = root("drop");
    let t = target(&root);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let demo = Demo {
        hold_at: Some((200, barrier.clone())),
        ..Demo::default()
    };
    let asked = demo.asked.clone();
    let job = download::download(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        vec![(day(5), Charge::Included), (day(6), Charge::Included)],
        now(),
        Cancel::new(),
    );
    barrier.wait(); // mid-day
    drop(job);
    barrier.wait();
    // The thread finishes on its own (no handle left to join): wait for the stored day.
    let part = t.dir().path.join("2026-11-05.lseg.part");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !t.dir().segment_path(day(5)).exists() || part.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the dropped job never finished its day"
        );
        std::thread::yield_now();
    }
    assert_eq!(lines_feed::disk::day::pinned(&t.dir().path), vec![day(5)]);
    nothing_left(&t.dir(), day(6));
    assert_eq!(
        asked.lock().unwrap().clone(),
        vec![day(5)],
        "day 6 never asked"
    );
}

/// Stop now mid-day: the day in flight leaves nothing; the request was reported first (a
/// paid day may be billed from there).
#[test]
fn stop_now_discards_the_day_in_flight() {
    let root = root("stop");
    let t = target(&root);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let demo = Demo {
        hold_at: Some((200, barrier.clone())),
        ..Demo::default()
    };
    let job = download::download(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        vec![(day(5), Charge::Included)],
        now(),
        Cancel::new(),
    );
    barrier.wait();
    job.cancel();
    barrier.wait();
    let updates = run(job);
    assert!(updates.contains(&Update::Requested { date: day(5) }));
    assert_eq!(outcomes(&updates), vec![(day(5), Outcome::Canceled)]);
    nothing_left(&t.dir(), day(5));
    assert!(lines_feed::disk::day::pinned(&t.dir().path).is_empty());
}

/// "Cancel remaining": the day in flight finishes and is stored; no later day starts.
#[test]
fn cancel_remaining_keeps_the_day_in_flight() {
    let root = root("remaining");
    let t = target(&root);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let demo = Demo {
        hold_at: Some((200, barrier.clone())),
        ..Demo::default()
    };
    let job = download::download(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        vec![(day(5), Charge::Included), (day(6), Charge::Included)],
        now(),
        Cancel::new(),
    );
    barrier.wait();
    job.cancel_remaining();
    barrier.wait();
    let out = outcomes(&run(job));
    assert!(
        matches!(out[0], (d, Outcome::Stored(_)) if d == day(5)),
        "{out:?}"
    );
    assert_eq!(out[1], (day(6), Outcome::Canceled));
    nothing_left(&t.dir(), day(6));
}

/// A day the provider delivers nothing for is not stored (it would be pinned as complete).
#[test]
fn an_empty_day_is_not_stored() {
    let root = root("empty");
    let t = target(&root);
    let demo = Demo {
        empty: true,
        ..Demo::default()
    };
    let out = get(demo, &t, &[(day(5), Charge::Included)]);
    assert_eq!(
        out[0].1,
        Outcome::Failed("No market data in this session.".into())
    );
    nothing_left(&t.dir(), day(5));
    assert!(lines_feed::disk::day::pinned(&t.dir().path).is_empty());
}

/// Stop now after everything paid has arrived (`fetched`) stops nothing: the day is converted
/// and stored, and counted as charged.
#[test]
fn stop_now_after_the_paid_data_arrived_still_stores_the_day() {
    let root = root("fetched");
    let t = target(&root);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let demo = Demo {
        hold_at: Some((200, barrier.clone())),
        fetched_first: true,
        ..Demo::default()
    };
    let job = download::download(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        vec![(day(5), Charge::Included)],
        now(),
        Cancel::new(),
    );
    barrier.wait();
    job.cancel();
    barrier.wait();
    let updates = run(job);
    assert!(updates.contains(&Update::Requested { date: day(5) }));
    assert!(
        matches!(&outcomes(&updates)[0], (d, Outcome::Stored(_)) if *d == day(5)),
        "{updates:?}"
    );
}

/// A source that never says when it asked, delivers something and then fails is still counted
/// as charged (the dialog never reads low), and nothing is stored.
#[test]
fn a_silent_source_that_fails_after_delivering_counts_as_charged() {
    let root = root("silent");
    let t = target(&root);
    let demo = Demo {
        silent_fail: true,
        ..Demo::default()
    };
    let updates = run(download::download(
        Arc::new(demo),
        t.clone(),
        download::demo::TOB.id,
        vec![(day(5), Charge::Included)],
        now(),
        Cancel::new(),
    ));
    assert!(updates.contains(&Update::Requested { date: day(5) }));
    assert_eq!(
        outcomes(&updates),
        vec![(day(5), Outcome::Failed("the connection dropped".into()))]
    );
    nothing_left(&t.dir(), day(5));
}
