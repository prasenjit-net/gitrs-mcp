//! Shared repository handle and the sync→async bridge for gix calls.

use std::path::Path;

/// A thread-safe handle to the served repository.
///
/// gix is synchronous and `gix::Repository` is `!Sync`, so every tool call runs
/// on the blocking pool with its own thread-local repository instance.
#[derive(Clone)]
pub struct RepoHandle {
    repo: gix::ThreadSafeRepository,
}

impl RepoHandle {
    pub fn discover(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            repo: gix::ThreadSafeRepository::discover(path)?,
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
