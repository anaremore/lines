//! Local logs (roadmap U1): `tracing` to stderr and to size-bounded rotating files, plus a
//! panic hook that writes the panic and a backtrace to the log before the process ends.
//!
//! Files: `<dir>/lines.log` (current), `lines.1.log` … `lines.<n-1>.log` (older); when the
//! current file would pass the size limit it becomes `lines.1.log` and the oldest is deleted.
//! Defaults: 5 files of 10 MB in `<data dir>/logs` (`lines_feed::disk::default_root`, i.e.
//! `%LOCALAPPDATA%\Lines\logs` on Windows). `LINES_LOG_DIR`, `LINES_LOG_FILES` (1–50) and
//! `LINES_LOG_MB` (1–1000) override them; `RUST_LOG` filters as before.
//!
//! Writing happens on its own thread (`lines-log`): a log call never does file I/O on the
//! UI thread. If the thread falls behind, lines are dropped and counted rather than blocking.
//! Every line is redacted (`crate::redact::text`) before it reaches the disk, so a credential
//! or account id that slipped into a message still never lands in a file. Screenshot and
//! benchmark runs write no log files.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::redact;

#[derive(Clone, Debug, PartialEq)]
pub struct LogConfig {
    pub dir: PathBuf,
    /// Files kept, the current one included.
    pub files: usize,
    /// Size at which the current file rotates.
    pub max_bytes: u64,
}

impl LogConfig {
    /// From the environment (see the module docs); `None` if there is no data directory.
    pub fn from_env() -> Option<LogConfig> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let dir = match var("LINES_LOG_DIR") {
            Some(d) => PathBuf::from(d),
            None => lines_feed::disk::default_root()?.join("logs"),
        };
        let files = var("LINES_LOG_FILES")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5usize)
            .clamp(1, 50);
        let mb = var("LINES_LOG_MB")
            .and_then(|v| v.parse().ok())
            .unwrap_or(10u64)
            .clamp(1, 1000);
        Some(LogConfig {
            dir,
            files,
            max_bytes: mb * 1024 * 1024,
        })
    }

    /// File `i` (0 = current).
    pub fn file(&self, i: usize) -> PathBuf {
        if i == 0 {
            self.dir.join("lines.log")
        } else {
            self.dir.join(format!("lines.{i}.log"))
        }
    }

    /// The log files that exist, newest first.
    pub fn existing(&self) -> Vec<PathBuf> {
        (0..self.files)
            .map(|i| self.file(i))
            .filter(|p| p.exists())
            .collect()
    }
}

/// The current file and its size; rotates by size.
struct Rotator {
    cfg: LogConfig,
    file: Option<File>,
    len: u64,
}

impl Rotator {
    fn open(cfg: LogConfig) -> std::io::Result<Rotator> {
        std::fs::create_dir_all(&cfg.dir)?;
        let path = cfg.file(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Rotator {
            cfg,
            file: Some(file),
            len,
        })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file = None;
        let n = self.cfg.files;
        if n <= 1 {
            let _ = std::fs::remove_file(self.cfg.file(0));
        } else {
            let _ = std::fs::remove_file(self.cfg.file(n - 1));
            for i in (0..n - 1).rev() {
                let from = self.cfg.file(i);
                if from.exists() {
                    std::fs::rename(&from, self.cfg.file(i + 1))?;
                }
            }
        }
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.cfg.file(0))?,
        );
        self.len = 0;
        Ok(())
    }

    /// Write one chunk of log text, redacted line by line, rotating first if it wouldn't fit.
    fn write(&mut self, text: &str) -> std::io::Result<()> {
        let mut clean = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let (body, nl) = line.split_at(line.trim_end_matches(['\r', '\n']).len());
            clean.push_str(&redact::text(body));
            clean.push_str(nl);
        }
        if self.len > 0 && self.len + clean.len() as u64 > self.cfg.max_bytes {
            self.rotate()?;
        }
        if let Some(f) = self.file.as_mut() {
            f.write_all(clean.as_bytes())?;
            self.len += clean.len() as u64;
        }
        Ok(())
    }

    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// The open log, shared by the writer thread and the panic hook.
static LOG: OnceLock<Arc<Mutex<Rotator>>> = OnceLock::new();
/// Where the logs go (for diagnostics bundles); unset when not logging to files.
static CONFIG: OnceLock<LogConfig> = OnceLock::new();

/// The log files' configuration, when logging to files.
pub fn config() -> Option<LogConfig> {
    CONFIG.get().cloned()
}
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Lines dropped because the writer thread fell behind (shown in diagnostics bundles).
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// `tracing`'s writer for the file layer: each event's bytes go to the writer thread.
#[derive(Clone)]
struct ChannelWriter {
    tx: SyncSender<Vec<u8>>,
}

struct Event {
    buf: Vec<u8>,
    tx: SyncSender<Vec<u8>>,
}

impl Write for Event {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        if let Err(TrySendError::Full(_)) = self.tx.try_send(std::mem::take(&mut self.buf)) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl<'a> MakeWriter<'a> for ChannelWriter {
    type Writer = Event;
    fn make_writer(&'a self) -> Event {
        Event {
            buf: Vec::new(),
            tx: self.tx.clone(),
        }
    }
}

fn writer_thread(rx: Receiver<Vec<u8>>, log: Arc<Mutex<Rotator>>) {
    while let Ok(first) = rx.recv() {
        let mut guard = log.lock().unwrap_or_else(|e| e.into_inner());
        let mut write = |b: Vec<u8>| {
            let _ = guard.write(&String::from_utf8_lossy(&b));
        };
        write(first);
        while let Ok(more) = rx.try_recv() {
            write(more);
        }
        guard.flush();
    }
}

/// Set up logging: stderr always, the rotating files when `files` is given (not in screenshot
/// or benchmark runs). A second call in one process (tests) keeps the first setup. Returns
/// the log directory in use.
pub fn init(files: Option<LogConfig>) -> Option<PathBuf> {
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,naga=warn,wgpu=warn".into())
    };
    let file = files.and_then(|cfg| match Rotator::open(cfg.clone()) {
        Ok(r) => Some((cfg, r)),
        Err(e) => {
            eprintln!(
                "logs: can't open {}: {e}; logging to stderr only",
                cfg.dir.display()
            );
            None
        }
    });
    let Some((cfg, rotator)) = file else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter())
            .try_init();
        return None;
    };
    let log = Arc::new(Mutex::new(rotator));
    let (tx, rx) = sync_channel::<Vec<u8>>(4096);
    let thread_log = log.clone();
    if std::thread::Builder::new()
        .name("lines-log".into())
        .spawn(move || writer_thread(rx, thread_log))
        .is_err()
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter())
            .try_init();
        return None;
    }
    let installed = tracing_subscriber::registry()
        .with(filter())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(ChannelWriter { tx }),
        )
        .try_init()
        .is_ok();
    if installed && LOG.set(log).is_ok() {
        let _ = CONFIG.set(cfg.clone());
        install_panic_hook();
        tracing::info!(
            "Lines {} ({}) logging to {} ({} × {} MB)",
            env!("CARGO_PKG_VERSION"),
            build_rev(),
            cfg.dir.display(),
            cfg.files,
            cfg.max_bytes / (1024 * 1024)
        );
        Some(cfg.dir)
    } else {
        None
    }
}

/// The source revision this binary was built from (`LINES_BUILD_REV`, set by `build.rs`).
pub fn build_rev() -> &'static str {
    option_env!("LINES_BUILD_REV").unwrap_or("unknown")
}

/// Panics are written to the log (message, location, backtrace) synchronously, then the
/// previous hook runs (it prints to stderr).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let text = format!("PANIC in thread '{name}': {info}\nbacktrace:\n{backtrace}\n");
        write_now(&text);
        previous(info);
    }));
}

/// Write straight to the log file, bypassing the writer thread (panics: the process may end
/// before the thread gets to it).
fn write_now(text: &str) {
    if let Some(log) = LOG.get() {
        let mut g = log.lock().unwrap_or_else(|e| e.into_inner());
        let _ = g.write(text);
        g.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lines-logs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn files_rotate_by_size_and_keep_at_most_n() {
        let dir = temp("rotate");
        let cfg = LogConfig {
            dir: dir.clone(),
            files: 3,
            max_bytes: 1000,
        };
        let mut r = Rotator::open(cfg.clone()).unwrap();
        for i in 0..200 {
            r.write(&format!("line {i:04} {}\n", "x".repeat(40)))
                .unwrap();
        }
        r.flush();
        let files = cfg.existing();
        assert_eq!(files.len(), 3, "{files:?}");
        for f in &files {
            assert!(std::fs::metadata(f).unwrap().len() <= 1000, "{f:?}");
        }
        // The newest lines are in the current file; the oldest were deleted.
        let current = std::fs::read_to_string(cfg.file(0)).unwrap();
        assert!(current.contains("line 0199"));
        let all: String = files
            .iter()
            .map(|f| std::fs::read_to_string(f).unwrap())
            .collect();
        assert!(!all.contains("line 0000"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lines_are_redacted_before_they_reach_the_disk() {
        let dir = temp("redact");
        let cfg = LogConfig {
            dir: dir.clone(),
            files: 2,
            max_bytes: 1 << 20,
        };
        let mut r = Rotator::open(cfg.clone()).unwrap();
        r.write("logon user=trader7 password=hunter2 ok\nfeed live\n")
            .unwrap();
        r.flush();
        let s = std::fs::read_to_string(cfg.file(0)).unwrap();
        assert!(!s.contains("hunter2") && !s.contains("trader7"), "{s}");
        assert!(s.contains("feed live\n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn file_names() {
        let c = LogConfig {
            dir: PathBuf::from("x"),
            files: 5,
            max_bytes: 10 * 1024 * 1024,
        };
        assert_eq!(c.file(0), std::path::Path::new("x").join("lines.log"));
        assert_eq!(c.file(3), std::path::Path::new("x").join("lines.3.log"));
    }
}
