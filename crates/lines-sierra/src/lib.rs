//! **Personal development tool — never ship.** Imports the owner's local Sierra Chart tick
//! (`.scid`) and market-depth (`.depth`) files into Lines `lines-events` replay files
//! (roadmap 3b). Sierra Chart withdrew its public depth-format documentation; the licence has
//! not been reviewed, so this crate is excluded from default builds and nothing in the app
//! depends on it. Imported data stays in `recordings/` (gitignored, decision 0010).

pub mod format;
pub mod import;
pub mod symbol;

#[cfg(test)]
mod import_tests;

pub use import::{ImportOptions, ImportReport, ImportSource, import, import_from};
