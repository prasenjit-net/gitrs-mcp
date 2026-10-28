//! Revisions, resolved in a real repository: `FUZZ_REPO` (default: the gitrs-mcp checkout this crate
//! lives in). Resolving never panics, whatever the spec.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

fn repo() -> &'static gix::ThreadSafeRepository {
    static REPO: OnceLock<gix::ThreadSafeRepository> = OnceLock::new();
    REPO.get_or_init(|| {
        let path = std::env::var("FUZZ_REPO").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/..").into());
        gix::ThreadSafeRepository::discover(path).expect("FUZZ_REPO is a git repository")
    })
}

fuzz_target!(|spec: &str| {
    let repo = repo().to_thread_local();
    let _ = gitrs_mcp::fuzzing::resolve_revision(&repo, spec);
});
