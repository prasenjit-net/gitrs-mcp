//! Tool implementations: sync functions over `&gix::Repository` returning
//! serde/schemars types that become the tools' structured output.

pub mod history;
pub mod refs;
pub mod repo;

use schemars::JsonSchema;
use serde::Serialize;

/// A person + timestamp, as recorded in a commit or tag.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
    /// UTC offset in seconds.
    pub offset: i32,
}

impl Signature {
    pub fn from_ref(sig: gix::actor::SignatureRef<'_>) -> anyhow::Result<Self> {
        let time = sig.time()?;
        Ok(Self {
            name: sig.name.to_string(),
            email: sig.email.to_string(),
            time: time.seconds,
            offset: time.offset,
        })
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::{path::Path, process::Command};

    /// Build a throwaway repo with the system `git`, which doubles as an oracle
    /// for gix behaving like real git.
    pub fn fixture_repo() -> (tempfile::TempDir, gix::Repository) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| run_git(dir.path(), args);
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "first commit"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(&["commit", "-q", "-am", "second commit\n\nwith a body"]);
        git(&["tag", "v1"]);
        git(&["branch", "feature"]);
        let repo = gix::open(dir.path()).unwrap();
        (dir, repo)
    }

    pub fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }
}
