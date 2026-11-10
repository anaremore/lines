//! Stable-Rust smoke fuzzing: every harness over its seeds and a few thousand deterministic
//! mutations of them. Runs on every PR (the libFuzzer runs are nightly and scheduled). A
//! failing input is written to `target/fuzz-smoke/<target>.bin` for `cargo fuzz run <target>
//! <file>` reproduction, and the failure message gives the run's seed, the mutation's number and
//! the input in hex (CI keeps no files). The mutations are deterministic: a target's seed is
//! fixed (`seed_for`), so the same seeds and iteration count give the same inputs on every
//! machine. `LINES_FUZZ_SMOKE_SEED` (decimal or `0x` hex) replaces it, to explore further or
//! replay a reported run.

use std::panic::{AssertUnwindSafe, catch_unwind};

use lines_fuzz::seeds::{self, Seed};

/// Mutations per target (more with `LINES_FUZZ_SMOKE_ITERS`).
fn iterations() -> usize {
    std::env::var("LINES_FUZZ_SMOKE_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500)
}

/// The RNG seed for `target`: `LINES_FUZZ_SMOKE_SEED` if set, else fixed per target.
fn seed_for(target: &str) -> u64 {
    match std::env::var("LINES_FUZZ_SMOKE_SEED") {
        Ok(v) => {
            let v = v.trim();
            let parsed = match v.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => v.parse(),
            };
            // Zero would stick the xorshift at zero.
            parsed
                .ok()
                .filter(|&n| n != 0)
                .unwrap_or_else(|| panic!("LINES_FUZZ_SMOKE_SEED={v:?} isn't a non-zero u64"))
        }
        Err(_) => 0x00f1_77ed_5eed ^ target.len() as u64,
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

const INTERESTING: [&[u8]; 10] = [
    &[0],
    &[0xff],
    &[0x7f],
    &[0x80],
    &[0xff, 0xff, 0xff, 0xff],
    &[0xff, 0xff, 0xff, 0x7f],
    &[0, 0, 0, 0x80],
    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
    &[0, 0, 0, 0, 0, 0, 0, 0x80],
    b"-9223372036854775808",
];

fn mutate(rng: &mut Rng, base: &[u8], other: &[u8]) -> Vec<u8> {
    let mut v = base.to_vec();
    for _ in 0..1 + rng.below(4) {
        let len = v.len();
        match rng.below(7) {
            0 if len > 0 => {
                let i = rng.below(len);
                v[i] ^= 1 << rng.below(8);
            }
            1 if len > 0 => {
                let i = rng.below(len);
                v[i] = rng.next() as u8;
            }
            2 => {
                let w = INTERESTING[rng.below(INTERESTING.len())];
                let i = rng.below(len + 1);
                let end = (i + w.len()).min(len);
                v.splice(i..end, w.iter().copied());
            }
            3 => v.truncate(rng.below(len + 1)),
            4 if len > 0 => {
                let (a, n) = (rng.below(len), 1 + rng.below(16));
                v.drain(a..(a + n).min(len));
            }
            5 if !other.is_empty() => {
                let i = rng.below(len + 1);
                let a = rng.below(other.len());
                let b = (a + 1 + rng.below(64)).min(other.len());
                v.splice(i..i, other[a..b].iter().copied());
            }
            _ => {
                let i = rng.below(len + 1);
                let n = rng.below(8);
                v.splice(i..i, (0..n).map(|_| rng.next() as u8));
            }
        }
    }
    v
}

fn run(target: &str, f: fn(&[u8])) {
    run_scaled(target, f, 1);
}

/// `divisor` cuts the mutation count for harnesses that do real file I/O per input.
fn run_scaled(target: &str, f: fn(&[u8]), divisor: usize) {
    let seeds: Vec<Seed> = seeds::all()
        .into_iter()
        .filter(|s| s.target == target)
        .collect();
    assert!(!seeds.is_empty(), "no seeds for {target}");
    let seed = seed_for(target);
    let check = |input: &[u8], what: &str| {
        if catch_unwind(AssertUnwindSafe(|| f(input))).is_err() {
            let dir =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fuzz-smoke");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("{target}.bin"));
            let _ = std::fs::write(&path, input);
            panic!(
                "{target} panicked on {what} (LINES_FUZZ_SMOKE_SEED={seed:#x}, {} seeds); input saved to {}; {} bytes: {}",
                seeds.len(),
                path.display(),
                input.len(),
                hex(input)
            );
        }
    };
    for (i, s) in seeds.iter().enumerate() {
        check(&s.data, &format!("seed {i}"));
    }
    let mut rng = Rng(seed);
    for i in 0..iterations() / divisor {
        let a = &seeds[rng.below(seeds.len())].data;
        let b = &seeds[rng.below(seeds.len())].data;
        check(&mutate(&mut rng, a, b), &format!("mutation {i}"));
    }
    // Plain noise and empty input.
    check(&[], "empty input");
    for n in [1, 7, 64, 4096] {
        let noise: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        check(&noise, &format!("{n} bytes of noise"));
    }
}

#[test]
fn cqg_decode() {
    run("cqg_decode", lines_fuzz::cqg_decode);
}

#[test]
fn cqg_session() {
    run("cqg_session", lines_fuzz::cqg_session);
}

#[test]
fn coinbase_session() {
    run("coinbase_session", lines_fuzz::coinbase_session);
}

#[test]
fn native_session() {
    run("native_session", lines_fuzz::native_session);
}

#[test]
fn recording() {
    run("recording", lines_fuzz::recording);
}

#[test]
fn sierra_import() {
    run("sierra_import", lines_fuzz::sierra_import);
}

#[test]
fn disk_store() {
    // Real files and a writer thread per input: a tenth of the mutations (libFuzzer does the
    // deep work nightly).
    run_scaled("disk_store", lines_fuzz::disk_store, 10);
}

#[test]
fn kraken_session() {
    run("kraken_session", lines_fuzz::kraken_session);
}

/// Seeds are only useful if the session accepts them: each session seed must leave bars and a
/// book in the store, not a rejected first frame.
#[test]
fn session_seeds_reach_deep_state() {
    for s in seeds::all() {
        let Some((bars, book, errors)) = lines_fuzz::session_depth(s.target, &s.data) else {
            continue;
        };
        if s.name == "pre-url-candles" {
            assert!(bars > 0, "{}/{}: no bars", s.target, s.name);
            continue;
        }
        assert!(bars > 0, "{}/{}: no bars", s.target, s.name);
        assert!(book > 0, "{}/{}: empty book", s.target, s.name);
        assert_eq!(errors, 0, "{}/{}: decode errors", s.target, s.name);
    }
}

#[test]
fn every_target_has_seeds_and_a_harness() {
    let all = seeds::all();
    for t in seeds::TARGETS {
        assert!(all.iter().any(|s| s.target == t), "{t}");
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../fuzz/fuzz_targets/{t}.rs")),
        )
        .unwrap();
        assert!(src.contains(&format!("lines_fuzz::{t}(")), "{t}");
    }
}
