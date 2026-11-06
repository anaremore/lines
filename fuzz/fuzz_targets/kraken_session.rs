#![no_main]
// Harness: lines_fuzz::kraken_session (also run by the stable smoke tests).
libfuzzer_sys::fuzz_target!(|data: &[u8]| lines_fuzz::kraken_session(data));
