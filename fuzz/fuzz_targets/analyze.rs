//! Feeds arbitrary input to the parser and analyzes and reports whatever
//! plan comes out, and anonymizes the input; none of it may panic.
//!
//! cargo +nightly fuzz run analyze fuzz/corpus/parse fixtures/pg/18 fixtures/inputs

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    if let Ok(plan) = explainsql_core::parse(&input) {
        let analysis = explainsql_core::analyze(&plan);
        let _ = explainsql_core::report::text(&plan, &analysis, true);
        let _ = explainsql_core::report::markdown(&plan, &analysis);
        let _ = explainsql_core::report::json(&plan, &analysis);
    }
    let _ = explainsql_core::anonymize::anonymize(&input, Default::default());
});
