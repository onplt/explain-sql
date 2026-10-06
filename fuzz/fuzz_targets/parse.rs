//! Feeds arbitrary input to the plan and log parsers, which must never
//! panic.
//!
//! cargo +nightly fuzz run parse fuzz/corpus/parse fixtures/pg/18 fixtures/inputs

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    let _ = explainsql_core::parse(&input);
    let _ = explainsql_core::parse_log(&input);
});
