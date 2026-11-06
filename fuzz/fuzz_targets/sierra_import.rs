#![no_main]
// Harness: lines_fuzz::sierra_import (also run by the stable smoke tests).
libfuzzer_sys::fuzz_target!(|data: &[u8]| lines_fuzz::sierra_import(data));
