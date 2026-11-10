//! Data folder tests: temp folders under the system temp dir (`TMP`), synthetic files only.

use super::*;

/// A fresh, empty folder for one test.
fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lines-data-dir-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(p: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// The tests' move id.
const MOVE: &str = "test-move";

/// The pointer, for a move from `from` (marked as its source when it is there, as choosing
/// the move does).
fn pointer(os: &Path, to: &Path, from: Option<&Path>) {
    if let Some(f) = from.filter(|f| f.is_dir()) {
        write_marker(f, MOVE).unwrap();
    }
    write_pointer(
        os,
        &Pointer {
            data_dir: to.to_path_buf(),
            pending_move: from.map(|f| PendingMove {
                from: f.to_path_buf(),
                id: MOVE.into(),
            }),
        },
    )
    .unwrap();
}

/// The record a background copy leaves for `from` → `to`, having written `written`.
fn record(os: &Path, from: &Path, to: &Path, written: &[&str]) {
    write_verified(
        os,
        &Verified {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            files: Vec::new(),
            written: written.iter().map(PathBuf::from).collect(),
        },
    )
    .unwrap();
}

/// A data folder with every item, plus a file of the user's that isn't Lines'.
fn populate(from: &Path) {
    write(
        &from.join("store-v1/coinbase/BTC-USD/2026-11-05.lseg"),
        &[7u8; 300_000],
    );
    write(&from.join("store-v1/coinbase/BTC-USD/bars"), b"bars");
    write(&from.join("store-v1/coinbase/BTC-USD/writer.lock"), b"");
    write(&from.join("logs/lines.log"), b"a log line\n");
    write(
        &from.join("recordings/mock-ES-20261105T140000Z.lfr"),
        b"lfr",
    );
    write(&from.join("journal/coinbase.jsonl"), b"{\"journal\":2}\n");
    write(&from.join("journal/shots/t1.svg"), b"<svg/>");
    write(&from.join("notes.txt"), b"the user's own file");
    write(&from.join("other/keep.bin"), b"not Lines'");
}

#[test]
fn env_beats_the_pointer_which_beats_the_default() {
    let d = dir("resolve");
    let (os, chosen, env) = (d.join("os"), d.join("chosen"), d.join("env"));
    std::fs::create_dir_all(&chosen).unwrap();
    let p = Some(Pointer {
        data_dir: chosen.clone(),
        pending_move: None,
    });
    let r = resolve(Some(env.clone()), Some(os.clone()), (p.clone(), None));
    assert_eq!((r.root, r.by), (Some(env), By::Env));
    let r = resolve(None, Some(os.clone()), (p, None));
    assert_eq!((r.root, r.by), (Some(chosen), By::Pointer));
    let r = resolve(None, Some(os.clone()), (None, None));
    assert_eq!((r.root, r.by, r.notice), (Some(os), By::Default, None));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_pointer_is_read_leniently() {
    let d = dir("lenient");
    let os = d.join("os");
    std::fs::create_dir_all(&os).unwrap();
    let path = pointer_path(&os);
    // Missing: the default, nothing said.
    assert_eq!(read_pointer(&path), (None, None));
    let r = resolve(None, Some(os.clone()), read_pointer(&path));
    assert_eq!((r.root, r.notice), (Some(os.clone()), None));
    // Garbage: the default, with a notice; the file is left as it is.
    write(&path, b"{ not json");
    let r = resolve(None, Some(os.clone()), read_pointer(&path));
    assert_eq!(r.root, Some(os.clone()));
    assert!(r.notice.unwrap().contains("couldn't be read"));
    assert_eq!(read(&path), b"{ not json");
    // Newer than this build: the default, with a notice.
    write(&path, br#"{"version": 9, "data_dir": "x"}"#);
    let r = resolve(None, Some(os.clone()), read_pointer(&path));
    assert_eq!(r.root, Some(os.clone()));
    assert!(r.notice.unwrap().contains("newer"));
    // A folder that doesn't exist: the default, with a notice, and nothing created.
    let gone = d.join("unplugged");
    pointer(&os, &gone, None);
    let r = resolve(None, Some(os.clone()), read_pointer(&path));
    assert_eq!((r.root, r.by), (Some(os.clone()), By::Default));
    assert!(r.notice.unwrap().contains("wasn't found"));
    assert!(!gone.exists(), "never created silently");
    // The saved form: version 1, the folder, no pending move.
    let text = String::from_utf8(read(&path)).unwrap();
    assert!(text.contains("\"version\": 1"), "{text}");
    assert!(!text.contains("pending_move"), "{text}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn paper_journal_and_backups_fall_back_to_the_config_folder() {
    let d = dir("legacy");
    let (data, config) = (d.join("data"), d.join("config"));
    // Neither has them: the data folder (a new install).
    let l = Locations::resolve(Some(&data), Some(&config));
    assert_eq!(l.paper, Some(data.join("paper")));
    assert_eq!(l.journal, Some(data.join("journal")));
    assert_eq!(l.backups, Some(data.join("backups")));
    // Only the config folder has them (an install from before): used where they are.
    std::fs::create_dir_all(config.join("paper")).unwrap();
    std::fs::create_dir_all(config.join("journal")).unwrap();
    let l = Locations::resolve(Some(&data), Some(&config));
    assert_eq!(l.paper, Some(config.join("paper")));
    assert_eq!(l.journal, Some(config.join("journal")));
    assert_eq!(l.backups, Some(data.join("backups")));
    // The data folder has one: it wins over the config folder's.
    std::fs::create_dir_all(data.join("paper")).unwrap();
    let l = Locations::resolve(Some(&data), Some(&config));
    assert_eq!(l.paper, Some(data.join("paper")));
    assert_eq!(l.journal, Some(config.join("journal")));
    // No data folder at all: the config folder, as before.
    let l = Locations::resolve(None, Some(&config));
    assert_eq!(l.backups, Some(config.join("backups")));
    assert_eq!(Locations::resolve(None, None), Locations::default());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_move_copies_checks_switches_then_removes_only_what_it_moved() {
    let d = dir("move");
    let (os, from, to, config) = (d.join("os"), d.join("old"), d.join("new"), d.join("config"));
    populate(&from);
    write(&config.join("paper/coinbase.json"), b"{\"paper\":1}");
    write(&config.join("backups/0.3.11/settings.json"), b"{}");
    write(&config.join("settings.json"), b"{\"version\":1}");
    std::fs::create_dir_all(&to).unwrap();
    write(&to.join("already-here.txt"), b"the user's");
    pointer(&os, &to, Some(&from));

    let (run, notices) = start(None, Some(os.clone()), Some(&config), true);
    assert_eq!(run.locations.root, Some(to.clone()), "{notices:?}");
    assert!(
        notices.iter().any(|n| n.starts_with("Data moved to")),
        "{notices:?}"
    );
    assert_eq!(run.problem, None);
    // Everything is in the new folder, the legacy config copies included.
    assert_eq!(
        read(&to.join("store-v1/coinbase/BTC-USD/2026-11-05.lseg")),
        vec![7u8; 300_000]
    );
    assert_eq!(read(&to.join("logs/lines.log")), b"a log line\n");
    assert_eq!(read(&to.join("journal/shots/t1.svg")), b"<svg/>");
    assert_eq!(read(&to.join("paper/coinbase.json")), b"{\"paper\":1}");
    assert_eq!(read(&to.join("backups/0.3.11/settings.json")), b"{}");
    assert_eq!(read(&to.join("already-here.txt")), b"the user's");
    assert_eq!(run.locations.paper, Some(to.join("paper")));
    // The pointer is switched: no move pending.
    let (p, _) = read_pointer(&pointer_path(&os));
    assert_eq!(p.unwrap().pending_move, None);
    // The old copies are gone; nothing else is.
    for item in ["store-v1", "logs", "recordings", "journal"] {
        assert!(!from.join(item).exists(), "{item} removed");
    }
    assert!(!config.join("paper").exists());
    assert!(!config.join("backups").exists());
    assert_eq!(read(&from.join("notes.txt")), b"the user's own file");
    assert_eq!(read(&from.join("other/keep.bin")), b"not Lines'");
    assert_eq!(read(&config.join("settings.json")), b"{\"version\":1}");
    assert!(from.is_dir(), "never the folder itself");
    assert!(
        !from.join(MARKER_FILE).exists(),
        "the marker goes with the move"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_failed_check_keeps_the_old_folder_and_both_copies() {
    let d = dir("verify");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    let src = from.join("logs/lines.log");
    // A copy of the same size and time with other bytes: copying skips it, the check
    // catches it.
    let dst = to.join("logs/lines.log");
    write(&dst, b"A LOG LINE\n");
    let t = std::fs::metadata(&src).unwrap().modified().unwrap();
    std::fs::File::options()
        .write(true)
        .open(&dst)
        .unwrap()
        .set_modified(t)
        .unwrap();
    record(&os, &from, &to, &["logs/lines.log"]);
    pointer(&os, &to, Some(&from));

    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(
        run.locations.root,
        Some(from.clone()),
        "still the old folder"
    );
    let problem = run.problem.unwrap();
    assert!(
        problem.starts_with("Failed: couldn't move data"),
        "{problem}"
    );
    assert!(problem.contains("differs after copying"), "{problem}");
    assert!(is_problem(&problem));
    assert!(
        problem.contains(&to.display().to_string()),
        "names the copy: {problem}"
    );
    assert_eq!(notices.len(), 1);
    // Both copies stay; the move is still pending (tried again at the next start).
    assert_eq!(read(&src), b"a log line\n");
    assert!(
        to.join("store-v1/coinbase/BTC-USD/2026-11-05.lseg")
            .exists()
    );
    let (p, _) = read_pointer(&pointer_path(&os));
    assert_eq!(p.unwrap().pending_move.unwrap().from, from);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn nested_folders_are_refused() {
    let d = dir("nested");
    let cur = d.join("data");
    std::fs::create_dir_all(cur.join("inner")).unwrap();
    assert!(check_target(&cur, &cur, None, None).is_err());
    assert_eq!(
        check_target(&cur, &cur.join("inner"), None, None).unwrap_err(),
        "That folder is inside the data folder"
    );
    assert_eq!(
        check_target(&cur, &d, None, None).unwrap_err(),
        "That folder contains the data folder"
    );
    std::fs::create_dir_all(d.join("elsewhere")).unwrap();
    assert_eq!(check_target(&cur, &d.join("elsewhere"), None, None), Ok(()));
    assert_eq!(probe_writable(&d.join("elsewhere")), Ok(()));
    assert!(
        std::fs::read_dir(d.join("elsewhere"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(probe_writable(&d.join("missing")).is_err());
    // A move between nested folders is refused at the start too.
    assert!(finish_move(&cur, MOVE, &cur.join("inner"), None, &mut None).is_err());
    assert!(finish_move(&cur.join("inner"), MOVE, &cur, None, &mut None).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_interrupted_copy_resumes_at_the_next_start() {
    let d = dir("resume");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    // The background copy got part way: one file whole, one cut short, a temporary left.
    let (done, cancel) = (AtomicU64::new(0), AtomicBool::new(false));
    copy_tree(&from.join("logs"), &to.join("logs"), &done, &cancel).unwrap();
    write(
        &to.join("store-v1/coinbase/BTC-USD/2026-11-05.lseg"),
        &[7u8; 1000],
    );
    write(&to.join("store-v1/coinbase/BTC-USD/bars.copy.tmp"), b"half");
    // A canceled copy stops early with what it had.
    cancel.store(true, Ordering::Relaxed);
    assert!(
        copy_tree(
            &from.join("recordings"),
            &to.join("recordings"),
            &done,
            &cancel
        )
        .is_err()
    );
    record(
        &os,
        &from,
        &to,
        &[
            "logs/lines.log",
            "store-v1/coinbase/BTC-USD/2026-11-05.lseg",
        ],
    );
    pointer(&os, &to, Some(&from));

    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(to.clone()), "{notices:?}");
    assert_eq!(
        read(&to.join("store-v1/coinbase/BTC-USD/2026-11-05.lseg")),
        vec![7u8; 300_000]
    );
    assert_eq!(
        read(&to.join("recordings/mock-ES-20261105T140000Z.lfr")),
        b"lfr"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn files_that_grew_after_the_background_copy_are_copied_again() {
    let d = dir("grew");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    let c = Copy {
        to: to.clone(),
        ..Copy::default()
    };
    background_copy(&from, &c, Some(&os), None);
    assert!(c.finished.load(Ordering::Relaxed));
    let rec = read_verified(&os, &from, &to).expect("the copy records what it checked");
    assert_eq!(rec.files.len(), 4, "{rec:?}");
    let total = c.total.load(Ordering::Relaxed);
    assert_eq!(c.done.load(Ordering::Relaxed), total);
    assert!(total >= 300_000, "the store, logs and recordings: {total}");
    assert!(
        !to.join("journal").exists(),
        "the small items wait for the restart"
    );
    // The segment grows while this run goes on.
    let seg = from.join("store-v1/coinbase/BTC-USD/2026-11-05.lseg");
    let mut grown = read(&seg);
    grown.extend_from_slice(&[9u8; 5000]);
    std::fs::write(&seg, &grown).unwrap();
    pointer(&os, &to, Some(&from));

    // A checked copy changed in the new folder (by size) is copied and checked again too.
    write(&to.join("recordings/mock-ES-20261105T140000Z.lfr"), b"lf");

    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(to.clone()), "{notices:?}");
    assert_eq!(
        read(&to.join("store-v1/coinbase/BTC-USD/2026-11-05.lseg")),
        grown
    );
    assert_eq!(
        read(&to.join("recordings/mock-ES-20261105T140000Z.lfr")),
        b"lfr"
    );
    assert!(
        !verified_path(&os).exists(),
        "the record goes with the switch"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Set `p`'s modification time to `src`'s.
fn same_time(p: &Path, src: &Path) {
    let t = std::fs::metadata(src).unwrap().modified().unwrap();
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// A file the background copy checked, still the same size and time in both folders, isn't
/// read again at the start: a different copy planted with that size and time stays (were it
/// read, the start would fail on it). The small items are always checked.
#[test]
fn files_checked_in_the_background_are_trusted_at_the_start() {
    let d = dir("trusted");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    let c = Copy {
        to: to.clone(),
        ..Copy::default()
    };
    background_copy(&from, &c, Some(&os), None);
    assert!(c.finished.load(Ordering::Relaxed));
    let planted = to.join("logs/lines.log");
    write(
        &planted,
        b"A LOG LINE
",
    );
    same_time(&planted, &from.join("logs/lines.log"));
    pointer(&os, &to, Some(&from));

    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(to.clone()), "{notices:?}");
    assert_eq!(
        read(&planted),
        b"A LOG LINE
",
        "trusted, not read again"
    );
    assert_eq!(read(&to.join("journal/shots/t1.svg")), b"<svg/>");
    let _ = std::fs::remove_dir_all(&d);
}

/// The record trusts only the big items, and only for its own move.
#[test]
fn the_record_never_covers_small_items_or_another_move() {
    let d = dir("record");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    let c = Copy {
        to: to.clone(),
        ..Copy::default()
    };
    background_copy(&from, &c, Some(&os), None);
    // The journal listed as checked, its copy planted with other bytes: still checked.
    let mut rec = read_verified(&os, &from, &to).unwrap();
    let src = from.join("journal/coinbase.jsonl");
    let meta = std::fs::metadata(&src).unwrap();
    rec.files.push(VerifiedFile {
        path: PathBuf::from("journal/coinbase.jsonl"),
        len: meta.len(),
        mtime_ns: mtime_ns(&meta).unwrap(),
        dst_len: None,
        dst_mtime_ns: None,
    });
    write_verified(&os, &rec).unwrap();
    let planted = to.join("journal/coinbase.jsonl");
    write(
        &planted,
        b"{\"JOURNAL\":2}
",
    );
    same_time(&planted, &src);
    pointer(&os, &to, Some(&from));
    let (run, _) = start(None, Some(os.clone()), None, true);
    assert_eq!(
        run.locations.root,
        Some(from.clone()),
        "the journal is checked"
    );
    assert!(run.problem.unwrap().contains("differs"));
    // Another move's record is no record: a planted log is caught.
    std::fs::copy(&src, &planted).unwrap();
    same_time(&planted, &src);
    let log = to.join("logs/lines.log");
    write(
        &log,
        b"A LOG LINE
",
    );
    same_time(&log, &from.join("logs/lines.log"));
    rec.to = d.join("elsewhere");
    write_verified(&os, &rec).unwrap();
    assert_eq!(read_verified(&os, &from, &to), None);
    let (run, _) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(from.clone()));
    let problem = run.problem.unwrap();
    assert!(problem.contains("already holds Lines data"), "{problem}");
    // Both copies stay.
    assert_eq!(
        read(&from.join("logs/lines.log")),
        b"a log line
"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_store_another_lines_is_writing_is_not_moved() {
    let d = dir("locked");
    let (from, to) = (d.join("old"), d.join("new"));
    populate(&from);
    std::fs::create_dir_all(&to).unwrap();
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(from.join("store-v1/coinbase/BTC-USD/writer.lock"))
        .unwrap();
    lock.lock().unwrap();
    write_marker(&from, MOVE).unwrap();
    let e = finish_move(&from, MOVE, &to, None, &mut None).unwrap_err();
    assert!(e.contains("another Lines"), "{e}");
    drop(lock);
    assert!(finish_move(&from, MOVE, &to, None, &mut None).is_ok());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn without_finishing_moves_a_run_uses_the_old_folder() {
    let d = dir("headless");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    std::fs::create_dir_all(&to).unwrap();
    pointer(&os, &to, Some(&from));
    let (run, notices) = start(None, Some(os.clone()), None, false);
    assert_eq!(run.locations.root, Some(from.clone()));
    assert!(notices.is_empty());
    assert!(!to.join("logs").exists(), "nothing copied");
    // With LINES_DATA_DIR the pointer isn't even read.
    let env = d.join("env");
    let (run, _) = start(Some(env.clone()), Some(os), None, true);
    assert_eq!((run.locations.root, run.by), (Some(env), By::Env));
    assert!(from.join("logs").exists(), "no move with LINES_DATA_DIR");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_move_waits_while_trading() {
    assert_eq!(move_refusal(0, 0), None);
    assert_eq!(
        move_refusal(1, 0).as_deref(),
        Some("Refused: not while a position or order is open (1 position open)")
    );
    assert_eq!(
        move_refusal(2, 3).as_deref(),
        Some("Refused: not while a position or order is open (2 positions open, 3 working orders)")
    );
    assert_eq!(
        move_refusal(0, 1).as_deref(),
        Some("Refused: not while a position or order is open (1 working order)")
    );
}

#[test]
fn flags_win_over_saved_store_settings_for_their_run() {
    let saved = StorePrefs {
        store: Some(false),
        budget_gb: Some(12.0),
        depth: Some(vec!["NQ".into()]),
    };
    let none = StoreFlags::default();
    assert_eq!(
        store_run(&none, &StorePrefs::default()),
        StoreRun {
            on: true,
            budget_gb: 4.0,
            depth: vec![],
        }
    );
    assert_eq!(
        store_run(&none, &saved),
        StoreRun {
            on: false,
            budget_gb: 12.0,
            depth: vec!["NQ".into()],
        }
    );
    let flags = StoreFlags {
        no_store: false,
        budget_gb: Some(2.5),
        depth: Some(vec!["ES".into()]),
        store_dir: true,
        store_dir_path: Some(PathBuf::from("D:/of-store")),
    };
    assert_eq!(
        store_run(&flags, &saved),
        StoreRun {
            on: true,
            budget_gb: 2.5,
            depth: vec!["ES".into()],
        }
    );
    let off = StoreFlags {
        no_store: true,
        ..StoreFlags::default()
    };
    assert!(!store_run(&off, &StorePrefs::default()).on);
    // A hand-edited budget that can't be used is the default.
    let bad = StorePrefs {
        budget_gb: Some(-1.0),
        ..StorePrefs::default()
    };
    assert_eq!(store_run(&none, &bad).budget_gb, 4.0);
}

#[test]
fn store_settings_are_peeked_from_settings_json() {
    let d = dir("peek");
    let path = d.join("settings.json");
    assert_eq!(StorePrefs::peek(&path), StorePrefs::default());
    write(
        &path,
        br#"{"version": 1, "store": false, "store_budget_gb": 20.0, "store_depth": ["ES", "ZZZ"]}"#,
    );
    assert_eq!(
        StorePrefs::peek(&path),
        StorePrefs {
            store: Some(false),
            budget_gb: Some(20.0),
            depth: Some(vec!["ES".into(), "ZZZ".into()]),
        }
    );
    write(&path, b"garbage");
    assert_eq!(StorePrefs::peek(&path), StorePrefs::default());
    assert_eq!(read(&path), b"garbage", "never set aside here");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_row_says_what_waits_for_the_restart() {
    let f = FolderFacts {
        in_use: Some(PathBuf::from("C:/Lines")),
        os_default: Some(PathBuf::from("C:/Lines")),
        ..FolderFacts::default()
    };
    assert_eq!(f.text(), "C:/Lines");
    assert_eq!(f.status(), None);
    assert!(f.changeable());
    let next = FolderFacts {
        next: Some(PathBuf::from("D:/LinesData")),
        ..f.clone()
    };
    assert_eq!(
        next.status().as_deref(),
        Some("D:/LinesData applies after restart (starts empty)")
    );
    let moving = FolderFacts {
        moving: true,
        copy: Some((1_288_490_189, 9_019_431_321, false)),
        ..next.clone()
    };
    assert_eq!(
        moving.status().as_deref(),
        Some("Moving to D:/LinesData after restart · Copying 1.2 of 8.4 GiB")
    );
    let env = FolderFacts { by: By::Env, ..f };
    assert_eq!(env.text(), "C:/Lines");
    assert_eq!(
        env.status().as_deref(),
        Some("Set by LINES_DATA_DIR for this run")
    );
    assert!(!env.changeable());
    assert_eq!(amount(120 << 20, 450 << 20), "120 of 450 MiB");
    assert_eq!(size(9_019_431_321), "8.4 GiB");
}

/// A move never deletes data when an item's source and target are one folder: the settings
/// folder picked as the target (or, on macOS, the default data folder that is the settings
/// folder) with legacy paper files in it.
#[test]
fn an_item_moved_onto_itself_is_refused_and_nothing_is_removed() {
    let d = dir("self");
    let (os, from, config) = (d.join("os"), d.join("old"), d.join("config"));
    std::fs::create_dir_all(from.join("logs")).unwrap();
    write(&from.join("logs/lines.log"), b"log");
    write(&config.join("paper/coinbase.json"), b"{\"paper\":1}");
    // Picking the settings folder is refused outright (unless it is the OS default).
    assert_eq!(
        check_target(&from, &config, Some(&config), Some(&os)).unwrap_err(),
        "That is the settings folder"
    );
    assert!(check_target(&from, &config.join("x"), Some(&config), Some(&os)).is_err());
    assert!(check_target(&from, &d, Some(&config), Some(&os)).is_err());
    assert_eq!(
        check_target(&from, &config, Some(&config), Some(&config)),
        Ok(())
    );
    // Were it moved there anyway (a record skips the holds-data check), the paper item's
    // source and target are one folder: refused, and the file stays.
    record(&os, &from, &config, &[]);
    pointer(&os, &config, Some(&from));
    let (run, _) = start(None, Some(os.clone()), Some(&config), true);
    assert_eq!(run.locations.root, Some(from.clone()));
    let problem = run.problem.unwrap();
    assert!(problem.contains("are the same folder"), "{problem}");
    assert_eq!(read(&config.join("paper/coinbase.json")), b"{\"paper\":1}");
    assert_eq!(read(&from.join("logs/lines.log")), b"log");
    // The same through a differently written path to the same folder.
    let mut rec = Some(Verified {
        from: from.clone(),
        to: config.clone(),
        ..Verified::default()
    });
    let other = PathBuf::from(config.display().to_string().replace('\\', "/"));
    assert!(finish_move(&from, MOVE, &other, Some(&config), &mut rec).is_err());
    assert!(config.join("paper/coinbase.json").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// A target already holding Lines data in any item is refused, at the pick and at the start,
/// and nothing there is overwritten; a file the move didn't write is never replaced.
#[test]
fn lines_data_already_in_the_target_is_never_overwritten() {
    for item in ITEMS {
        let d = dir(&format!("collide-{item}"));
        let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
        populate(&from);
        write(&from.join("paper/coinbase.json"), b"old paper");
        write(&from.join("backups/0.3.11/settings.json"), b"old backup");
        let theirs = to.join(item).join("theirs.bin");
        write(&theirs, b"the target's own");
        assert_eq!(holds_data(&to), Some(item));
        let why = check_pick(&from, &to, None, None).unwrap().unwrap();
        assert!(
            why.starts_with("Refused:") && why.contains(item_words(item)),
            "{why}"
        );
        assert!(
            !why.contains("(store-v1)"),
            "no folder names in words: {why}"
        );
        pointer(&os, &to, Some(&from));
        let (run, _) = start(None, Some(os.clone()), None, true);
        assert_eq!(run.locations.root, Some(from.clone()), "{item}");
        assert!(run.problem.unwrap().contains("already holds Lines data"));
        assert_eq!(read(&theirs), b"the target's own");
        let _ = std::fs::remove_dir_all(&d);
    }
    // With this move's record, a colliding file it didn't write fails the move, unchanged.
    for (item, rel) in [
        ("store-v1", "store-v1/coinbase/BTC-USD/bars"),
        ("logs", "logs/lines.log"),
        ("recordings", "recordings/mock-ES-20261105T140000Z.lfr"),
        ("journal", "journal/coinbase.jsonl"),
        ("paper", "paper/coinbase.json"),
        ("backups", "backups/0.3.11/settings.json"),
    ] {
        let d = dir(&format!("overwrite-{item}"));
        let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
        populate(&from);
        write(&from.join("paper/coinbase.json"), b"old paper");
        write(&from.join("backups/0.3.11/settings.json"), b"old backup");
        write(&to.join(rel), b"DIFFERENT");
        record(&os, &from, &to, &[]);
        pointer(&os, &to, Some(&from));
        let (run, _) = start(None, Some(os.clone()), None, true);
        assert_eq!(run.locations.root, Some(from.clone()), "{item}");
        let problem = run.problem.unwrap();
        assert!(problem.contains("already exists and differs"), "{problem}");
        assert_eq!(read(&to.join(rel)), b"DIFFERENT", "{item} not overwritten");
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// An unplugged source at the restart: the move fails and stays pending, nothing switches.
#[test]
fn a_missing_source_fails_the_move_and_keeps_it_pending() {
    let d = dir("unplugged");
    let (os, from, to) = (d.join("os"), d.join("gone"), d.join("new"));
    std::fs::create_dir_all(&to).unwrap();
    pointer(&os, &to, Some(&from));
    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(from.clone()));
    let problem = run.problem.unwrap();
    assert!(problem.contains("wasn't found"), "{problem}");
    assert!(
        problem.contains(&format!(
            "this run uses an empty folder at {}",
            from.display()
        )),
        "{problem}"
    );
    assert_eq!(notices.len(), 1);
    let (p, _) = read_pointer(&pointer_path(&os));
    assert_eq!(p.unwrap().pending_move.unwrap().from, from);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn paths_never_break_when_a_line_wraps() {
    let paths = vec!["C:/My Data/Lines".to_owned()];
    assert_eq!(
        wrap_tokens("Moving to C:/My Data/Lines after restart", &paths),
        vec!["Moving", "to", "C:/My Data/Lines", "after", "restart"]
    );
    assert_eq!(
        wrap_tokens("still using (C:/My Data/Lines); the copy", &paths),
        vec!["still", "using", "(C:/My Data/Lines);", "the", "copy"]
    );
    assert_eq!(
        wrap_tokens("D:\\x-y\\z is kept", &[]),
        vec!["D:\\x-y\\z", "is", "kept"]
    );
}

/// Choices are written in order: one made before a newer one doesn't write.
#[test]
fn an_older_choice_never_writes_over_a_newer_one() {
    let a = next_seq();
    let b = next_seq();
    assert_eq!(ordered(Some(a), || 1), None, "superseded");
    assert_eq!(ordered(Some(b), || 2), Some(2));
    assert_eq!(ordered(None, || 3), Some(3));
}

/// exFAT and FAT drives keep file times in steps: the copy's own time, as the drive stored it,
/// is what the record compares the copy with, so a checked file isn't read again.
#[test]
fn a_copy_on_a_coarse_clock_drive_is_still_trusted() {
    let d = dir("exfat");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    let c = Copy {
        to: to.clone(),
        ..Copy::default()
    };
    background_copy(&from, &c, Some(&os), None);
    // The background copy records the copy's own time.
    let rec = read_verified(&os, &from, &to).unwrap();
    let log = rec
        .files
        .iter()
        .find(|f| f.path == Path::new("logs").join("lines.log"))
        .unwrap();
    let copy = to.join("logs/lines.log");
    assert_eq!(
        log.dst_mtime_ns,
        mtime_ns(&std::fs::metadata(&copy).unwrap())
    );
    // As exFAT stores it: the time rounded down to 2 s, recorded as the drive wrote it; a
    // planted copy with that size and time stays (were it read, the start would fail).
    let mut rec = rec;
    let src_t = std::fs::metadata(from.join("logs/lines.log"))
        .unwrap()
        .modified()
        .unwrap();
    let secs = src_t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let rounded = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs - secs % 2);
    write(
        &copy,
        b"A LOG LINE
",
    );
    std::fs::File::options()
        .write(true)
        .open(&copy)
        .unwrap()
        .set_modified(rounded)
        .unwrap();
    for f in rec
        .files
        .iter_mut()
        .filter(|f| f.path.ends_with("lines.log"))
    {
        f.dst_mtime_ns = mtime_ns(&std::fs::metadata(&copy).unwrap());
    }
    write_verified(&os, &rec).unwrap();
    assert_ne!(
        mtime_ns(&std::fs::metadata(&copy).unwrap()),
        mtime_ns(&std::fs::metadata(from.join("logs/lines.log")).unwrap()),
        "the copy's time differs from the source's"
    );
    pointer(&os, &to, Some(&from));
    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(to.clone()), "{notices:?}");
    assert_eq!(
        read(&copy),
        b"A LOG LINE
",
        "trusted, not copied again"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A source file gone between the listing and its copy (a rotated log, a date the budget
/// removed) is skipped: the background copy goes on.
#[test]
fn a_source_file_that_vanishes_is_skipped() {
    let d = dir("vanish");
    let (from, to) = (d.join("old"), d.join("new"));
    std::fs::create_dir_all(&from).unwrap();
    let c = Copy {
        to: to.clone(),
        ..Copy::default()
    };
    let mut v = Verified::default();
    let key = PathBuf::from("logs/lines.1.log");
    let r = background_file(&from.join(&key), &to.join(&key), key.clone(), &mut v, &c);
    assert_eq!(r, Ok(()));
    assert!(v.files.is_empty(), "nothing recorded as checked");
    assert!(!to.join(&key).exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// "Use its data": a folder already holding Lines data is used as it is from the next start;
/// nothing is copied into it or written over.
#[test]
fn using_a_folder_with_data_never_writes_over_it() {
    let d = dir("use-its-data");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    populate(&to);
    write(&to.join("journal/coinbase.jsonl"), b"the target's journal");
    let before: Vec<(PathBuf, Vec<u8>)> = walk(&to)
        .unwrap()
        .into_iter()
        .map(|r| (r.clone(), read(&to.join(&r))))
        .collect();
    // What choosing it writes: the pointer only, no move.
    pointer(&os, &to, None);
    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(to.clone()));
    assert!(notices.is_empty(), "{notices:?}");
    for (r, bytes) in before {
        assert_eq!(read(&to.join(&r)), bytes, "{r:?} untouched");
    }
    assert!(!verified_path(&os).exists(), "no move record");
    // The row says the data is used, not "starts empty".
    let f = FolderFacts {
        in_use: Some(from.clone()),
        os_default: Some(os.clone()),
        next: Some(to.clone()),
        next_holds: true,
        ..FolderFacts::default()
    };
    let line = f.status().unwrap();
    assert!(
        line.starts_with("After restart: uses the data already in"),
        "{line}"
    );
    assert!(!line.contains("empty"));
    let _ = std::fs::remove_dir_all(&d);
}

/// With LINES_DATA_DIR the row names the folder Settings saved, read without using it.
#[test]
fn the_env_row_names_the_saved_folder() {
    let d = dir("env-saved");
    let (os, saved, env) = (d.join("os"), d.join("saved"), d.join("env"));
    std::fs::create_dir_all(&saved).unwrap();
    pointer(&os, &saved, None);
    let (run, _) = start(Some(env.clone()), Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(env.clone()));
    assert_eq!(run.saved, Some(saved.clone()));
    let f = FolderFacts {
        in_use: Some(env),
        by: By::Env,
        saved: Some(saved.clone()),
        ..FolderFacts::default()
    };
    assert_eq!(
        f.status().unwrap(),
        format!(
            "Set by LINES_DATA_DIR for this run · saved: {}",
            saved.display()
        )
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// No sentence ends with a period hard against a path.
#[test]
fn no_period_hard_against_a_path() {
    let d = dir("period");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    write(&to.join("logs/x"), b"x");
    pointer(&os, &to, Some(&from));
    let (run, _) = start(None, Some(os.clone()), None, true);
    let p = run.problem.unwrap();
    for path in [&from, &to] {
        assert!(!p.contains(&format!("{}.", path.display())), "{p}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A source missing at a start, recreated empty by that run (or a different disk on its
/// letter, or a bare mount point), is never moved from: no marker, no switch, nothing moved.
/// The real source coming back with its marker finishes the move.
#[test]
fn only_the_marked_source_is_ever_moved_from() {
    let d = dir("marker");
    let (os, from, to) = (d.join("os"), d.join("old"), d.join("new"));
    populate(&from);
    std::fs::create_dir_all(&to).unwrap();
    pointer(&os, &to, Some(&from));
    // The drive is unplugged at the start: this run uses an empty folder at its path ...
    let away = d.join("old-away");
    std::fs::rename(&from, &away).unwrap();
    let (run, _) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(from.clone()));
    // ... and recreates it (its logs, a paper save).
    write(&from.join("paper/coinbase.json"), b"fresh paper");
    let (run, _) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(from.clone()), "no switch");
    let problem = run.problem.unwrap();
    assert!(
        problem.contains("isn't the folder being moved from (its marker is missing)"),
        "{problem}"
    );
    let (p, _) = read_pointer(&pointer_path(&os));
    assert!(p.unwrap().pending_move.is_some(), "still pending");
    assert_eq!(read(&from.join("paper/coinbase.json")), b"fresh paper");
    assert!(!to.join("paper").exists(), "nothing moved");
    // An empty mount-point-style folder from the start: no switch either.
    let mount = d.join("mount");
    std::fs::create_dir_all(&mount).unwrap();
    write_pointer(
        &os,
        &Pointer {
            data_dir: to.clone(),
            pending_move: Some(PendingMove {
                from: mount.clone(),
                id: MOVE.into(),
            }),
        },
    )
    .unwrap();
    let (run, _) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(mount.clone()));
    assert!(run.problem.unwrap().contains("marker is missing"));
    // The real source comes back with its marker: the move finishes.
    let _ = std::fs::remove_dir_all(&from);
    std::fs::rename(&away, &from).unwrap();
    pointer(&os, &to, Some(&from));
    let (run, notices) = start(None, Some(os.clone()), None, true);
    assert_eq!(run.locations.root, Some(to.clone()), "{notices:?}");
    assert!(to.join("journal/coinbase.jsonl").exists());
    assert!(!from.join(MARKER_FILE).exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// The folder facts cache keeps only the facts for the key being worked out: a worker that
/// finishes for an older pointer never replaces a newer one's, and the key is then done.
#[test]
fn stale_folder_facts_are_dropped() {
    let key = |s: &str| Some(PathBuf::from(s));
    let facts = |k: Option<PathBuf>| Derived {
        key: k,
        ..Derived::default()
    };
    *DERIVING.lock().unwrap() = Some(key("new"));
    assert!(!store_derived(facts(key("old"))), "stale: dropped");
    assert_eq!(*DERIVING.lock().unwrap(), Some(key("new")));
    assert!(store_derived(facts(key("new"))));
    assert_eq!(*DERIVING.lock().unwrap(), None, "done");
    assert_eq!(
        DERIVED.lock().unwrap().as_ref().map(|d| d.key.clone()),
        Some(key("new"))
    );
}

/// Alt+D (and its button): only when the folder can change, nothing waits, and the default
/// isn't in use.
#[test]
fn use_the_default_folder_is_offered_only_when_it_can_apply() {
    let f = FolderFacts {
        in_use: Some(PathBuf::from("D:/LinesData")),
        os_default: Some(PathBuf::from("C:/Lines")),
        ..FolderFacts::default()
    };
    assert!(f.can_use_default());
    let on_default = FolderFacts {
        on_default: true,
        ..f.clone()
    };
    assert!(!on_default.can_use_default());
    let waiting = FolderFacts {
        next: Some(PathBuf::from("E:/x")),
        ..f.clone()
    };
    assert!(!waiting.can_use_default());
    let env = FolderFacts { by: By::Env, ..f };
    assert!(!env.can_use_default());
}

/// With --store-dir, the Data folder row says where this run's store is.
#[test]
fn the_folder_row_names_a_store_dir() {
    use crate::settings_schema::{Facts, Prefs, Target, by_key, status};
    let mut f = Facts::default();
    let st = by_key(crate::settings_schema::DATA_FOLDER_KEY).unwrap();
    assert_eq!(status(st, &Prefs::default(), &Target::default(), &f), None);
    f.store_flags.store_dir_path = Some(PathBuf::from("D:/of-store"));
    assert_eq!(
        status(st, &Prefs::default(), &Target::default(), &f).as_deref(),
        Some("Its store is at D:/of-store for this run (--store-dir)")
    );
}
