#![no_main]
// Harness: lines_fuzz::recording (also run by the stable smoke tests).
libfuzzer_sys::fuzz_target!(|data: &[u8]| lines_fuzz::recording(data));
