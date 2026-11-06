#![no_main]
// Harness: lines_fuzz::cqg_decode (also run by the stable smoke tests).
libfuzzer_sys::fuzz_target!(|data: &[u8]| lines_fuzz::cqg_decode(data));
