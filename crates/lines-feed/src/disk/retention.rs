//! Retention: keep the store under a size budget by deleting the oldest trade dates first
//! (decision 0019). Segments being written and downloaded days (`day::PINNED`, decision 0030
//! §8: a paid day is fetched once) are never deleted; they still count toward the budget.
//!
//! Only segment files count and are evicted; the bar index and coverage files are small and
//! stay (the bar index is a cache; coverage claims for trades are always read together with
//! which segment files still exist, see `ContractDir::trade_coverage`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Segment files open for writing in this process (never evicted).
static ACTIVE: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

pub(crate) fn mark_active(path: &Path, active: bool) {
    let mut set = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if active {
        set.insert(path.to_path_buf());
    } else {
        set.remove(path);
    }
}

fn is_active(path: &Path) -> bool {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(path)
}

/// A downloaded day (listed in its contract's `downloaded` file): never evicted.
fn is_pinned(path: &Path) -> bool {
    let (Some(dir), Some(date)) = (
        path.parent(),
        path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(super::parse_date),
    ) else {
        return false;
    };
    super::day::pinned(dir).contains(&date)
}

/// Every segment under `root`: (trade date `YYYY-MM-DD` from the file name, path, bytes).
pub fn segments(root: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == super::SEGMENT_EXT)
                && let Some(date) = p.file_stem().and_then(|s| s.to_str())
            {
                out.push((date.to_owned(), p, meta.len()));
            }
        }
    }
    out
}

/// What [`enforce`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evicted {
    pub files: Vec<PathBuf>,
    pub bytes: u64,
    /// Bytes still stored.
    pub kept: u64,
}

/// Delete the oldest trade dates (across all contracts) until the store fits `budget` bytes.
/// Files being written and downloaded days (pinned) are skipped, though they count; a file
/// that can't be deleted (e.g. open elsewhere) stays.
pub fn enforce(root: &Path, budget: u64) -> Evicted {
    let mut segs = segments(root);
    let mut total: u64 = segs.iter().map(|s| s.2).sum();
    let mut out = Evicted::default();
    // Oldest date first; ties broken by path so the order is deterministic.
    segs.sort();
    for (_, path, bytes) in segs {
        if total <= budget {
            break;
        }
        if is_active(&path) || is_pinned(&path) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total -= bytes;
                out.bytes += bytes;
                out.files.push(path);
            }
            Err(e) => tracing::warn!("store retention: can't delete {}: {e}", path.display()),
        }
    }
    out.kept = total;
    if !out.files.is_empty() {
        tracing::info!(
            "store retention: removed {} old segment(s), {} bytes; {} bytes kept (budget {budget})",
            out.files.len(),
            out.bytes,
            total
        );
    }
    out
}
