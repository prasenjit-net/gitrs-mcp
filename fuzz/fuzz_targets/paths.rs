//! Repository-relative paths, as every tool receives them: normalizing never panics, and a path it
//! accepts can't escape the repository.
#![no_main]

use gitrs_mcp::fuzzing::{is_git_dir_name, repo_path};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if let Ok(path) = repo_path(input) {
        let text = path.to_string();
        assert!(!text.starts_with('/') && !text.starts_with('\\'), "{input:?} -> {text:?}");
        assert!(text.split('/').all(|part| part != ".." && part != "."), "{input:?} -> {text:?}");
        assert_eq!(repo_path(&text).ok().as_ref(), Some(&path), "not idempotent: {input:?}");
    }
    for component in input.as_bytes().split(|b| *b == b'/') {
        let _ = is_git_dir_name(component);
    }
});
