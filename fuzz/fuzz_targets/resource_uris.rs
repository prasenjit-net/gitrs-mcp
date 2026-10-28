//! `gitrs://` resource URIs, with their `?repo=` query: parsing never panics.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|uri: &str| {
    let _ = gitrs_mcp::fuzzing::parse_resource_uri(uri);
});
