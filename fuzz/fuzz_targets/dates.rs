//! `since` / `until` dates for search_commits: parsing never panics.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let _ = gitrs_mcp::fuzzing::parse_date(input);
});
