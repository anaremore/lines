//! Scratch folders for tests and fixtures: a uniquely named folder under the system temp
//! folder, removed with everything in it when the guard drops, a failed test's unwinding
//! included. Every test, fixture and screenshot run that writes files uses one, so test runs
//! leave nothing behind in `%TEMP%` (they once left thousands of `lines-*` folders).

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The prefix every scratch folder's name starts with (`lines-<name>-<pid>-<n>`).
pub const PREFIX: &str = "lines-";

/// A scratch folder (created, empty), or a file path inside one: derefs to that path. The
/// folder goes when this drops.
#[derive(Debug)]
pub struct Scratch {
    root: PathBuf,
    path: PathBuf,
}

impl Scratch {
    /// A new, empty folder `lines-<name>-<pid>-<n>` under the system temp folder (unique in
    /// this process, so tests running in parallel never share one).
    pub fn new(name: &str) -> Scratch {
        debug_assert!(
            !name.contains(['/', '\\']) && !name.contains(".."),
            "a scratch name is one folder name: {name:?}"
        );
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("{PREFIX}{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)
            .unwrap_or_else(|e| panic!("scratch folder {}: {e}", root.display()));
        Scratch {
            path: root.clone(),
            root,
        }
    }

    /// A new scratch folder, standing for the file `file` inside it (not created).
    pub fn file(name: &str, file: &str) -> Scratch {
        let mut s = Scratch::new(name);
        s.path = s.root.join(file);
        s
    }

    /// The folder (removed on drop).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder, or the file this stands for.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keep the folder (a failed run's evidence): it is no longer removed, and its path is
    /// returned to report.
    pub fn keep(mut self) -> PathBuf {
        let root = std::mem::take(&mut self.root);
        std::mem::forget(self);
        root
    }
}

impl Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Windows can hold a just-closed file for a moment (an indexer, a virus scanner): a
        // few tries, then leave it, said on stderr (a failing test's output shows it), rather
        // than panic in a drop.
        for k in 0..5 {
            match std::fs::remove_dir_all(&self.root) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    if k == 4 {
                        eprintln!("scratch folder {} left: {e}", self.root.display());
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
                _ => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_folder_goes_when_dropped_even_on_a_panic() {
        let s = Scratch::new("scratch-self");
        let root = s.root().to_path_buf();
        assert!(root.is_dir());
        assert!(
            root.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(PREFIX)
        );
        std::fs::create_dir_all(s.join("a/b")).unwrap();
        std::fs::write(s.join("a/b/c.txt"), b"x").unwrap();
        drop(s);
        assert!(!root.exists());

        let f = Scratch::file("scratch-file", "x.lfr");
        assert_eq!(f.path().parent(), Some(f.root()));
        let root = f.root().to_path_buf();
        let r = std::panic::catch_unwind(move || {
            let _f = f;
            panic!("a failing test");
        });
        assert!(r.is_err());
        assert!(!root.exists(), "removed while unwinding");

        // Kept: left in place, its path returned.
        let k = Scratch::new("scratch-keep").keep();
        assert!(k.is_dir());
        std::fs::remove_dir_all(&k).unwrap();

        // Two at once, same name: never the same folder.
        let (a, b) = (Scratch::new("scratch-two"), Scratch::new("scratch-two"));
        assert_ne!(a.root(), b.root());
    }
}
