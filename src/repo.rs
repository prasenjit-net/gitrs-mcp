//! Shared repository handle and the sync→async bridge for gix calls.

use std::path::Path;

use anyhow::Context;

/// A thread-safe handle to the served repository.
///
/// gix is synchronous and `gix::Repository` is `!Sync`, so every tool call runs
/// on the blocking pool with its own thread-local repository instance.
#[derive(Clone)]
pub struct RepoHandle {
    repo: gix::ThreadSafeRepository,
}

impl RepoHandle {
    /// Find the repository containing `path` and make its root the process working directory.
    ///
    /// gix resolves pathspecs against the working directory captured when a repository is opened.
    /// Tool arguments are documented as relative to the repository root, so a server launched from a
    /// subdirectory would otherwise silently shift every path. Changing directory and reopening keeps
    /// them root-relative.
    pub fn discover(path: &Path) -> anyhow::Result<Self> {
        let found = gix::ThreadSafeRepository::discover(path)?;
        // Discovery can return a relative root such as `..`; make it absolute before moving into it.
        let root = found.work_dir().unwrap_or_else(|| found.git_dir());
        let root = std::fs::canonicalize(root)
            .with_context(|| format!("cannot resolve repository root {}", root.display()))?;
        std::env::set_current_dir(&root)
            .with_context(|| format!("cannot change directory to {}", root.display()))?;
        Ok(Self {
            repo: gix::ThreadSafeRepository::open(&root)?,
        })
    }

    pub fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    /// Run `f` against the repository on the blocking pool.
    ///
    /// Errors are returned as `String` so rmcp reports them as tool-level errors
    /// (`isError: true`) the model can read and react to, not protocol failures.
    pub async fn run<T, F>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(&gix::Repository) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let repo = self.repo.clone();
        tokio::task::spawn_blocking(move || f(&repo.to_thread_local()))
            .await
            .map_err(|e| format!("internal error: {e}"))?
            .map_err(|e| format!("{e:#}"))
    }
}
