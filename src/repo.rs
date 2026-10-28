//! Repository handles and the sync→async bridge for gix calls.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context;

/// Object cache of each thread-local repository. Objects are immutable, so a warm cache never goes
/// stale; it grows gradually up to this size.
const OBJECT_CACHE_BYTES: usize = 16 * 1024 * 1024;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The thread-local repositories of the threads that run tool calls, by handle id, kept between
    /// calls so their object caches stay warm.
    static OPEN: RefCell<HashMap<u64, gix::Repository>> = RefCell::new(HashMap::new());
}

/// A thread-safe handle to one served repository.
///
/// gix is synchronous and `gix::Repository` is `!Sync`, so every tool call runs
/// on the blocking pool with its own thread-local repository instance.
#[derive(Clone)]
pub struct RepoHandle {
    repo: gix::ThreadSafeRepository,
    root: Arc<PathBuf>,
    /// Distinguishes handles in the thread-local cache; clones share it.
    id: u64,
}

impl RepoHandle {
    /// The root of the repository containing `path` (searching parent directories like git): its
    /// working tree, or the git directory of a bare repository. Absolute and canonical.
    pub fn find_root(path: &Path) -> anyhow::Result<PathBuf> {
        let found = gix::ThreadSafeRepository::discover(path)
            .with_context(|| format!("no git repository found at or above {}", path.display()))?;
        // Discovery can return a relative root such as `..`.
        let root = found.work_dir().unwrap_or_else(|| found.git_dir());
        std::fs::canonicalize(root)
            .with_context(|| format!("cannot resolve repository root {}", root.display()))
    }

    /// Make the filesystem root the process working directory, before any repository is opened.
    ///
    /// gix resolves pathspecs against the working directory captured when a repository is opened,
    /// relative to the working tree when it's inside one. Tool arguments are documented as relative
    /// to the repository root, so a server launched from a subdirectory would silently shift every
    /// path. From outside every working tree, each repository resolves paths from its own root.
    pub fn leave_worktrees(near: &Path) -> anyhow::Result<()> {
        let top = near.ancestors().last().unwrap_or(near);
        std::env::set_current_dir(top)
            .with_context(|| format!("cannot change directory to {}", top.display()))
    }

    /// Open the repository whose root is `root` (from [`Self::find_root`]).
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            repo: gix::ThreadSafeRepository::open(&root)
                .with_context(|| format!("cannot open the repository at {}", root.display()))?,
            root: Arc::new(root),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Open the repository at `path` with global and system config ignored, without changing the
    /// process directory (which tests share), for exercising the MCP layer.
    #[cfg(test)]
    pub fn open_isolated(path: &Path) -> Self {
        Self {
            repo: gix::ThreadSafeRepository::open_opts(path, gix::open::Options::isolated())
                .expect("test repository opens"),
            root: Arc::new(std::fs::canonicalize(path).expect("test repository path resolves")),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// The repository's root, as found by [`Self::find_root`].
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    pub fn is_bare(&self) -> bool {
        self.repo.work_dir().is_none()
    }

    /// Run `f` against the repository on the calling thread, for quick work in a synchronous
    /// callback that can't wait for the blocking pool.
    pub fn run_now<T>(&self, f: impl FnOnce(&gix::Repository) -> T) -> T {
        with_local(&self.repo, self.id, f)
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
        let (repo, id) = (self.repo.clone(), self.id);
        tokio::task::spawn_blocking(move || with_local(&repo, id, f))
            .await
            .map_err(|e| format!("internal error: {e}"))?
            .map_err(|e| format!("{e:#}"))
    }
}

/// Run `f` on this thread's repository for handle `id`, opening it (with an object cache) the first
/// time. It's out of the cache while `f` runs, so a nested call or a panic can't see it borrowed.
fn with_local<T>(
    shared: &gix::ThreadSafeRepository,
    id: u64,
    f: impl FnOnce(&gix::Repository) -> T,
) -> T {
    let cached = OPEN.with(|open| open.borrow_mut().remove(&id));
    let repo = cached.unwrap_or_else(|| {
        let mut repo = shared.to_thread_local();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        repo
    });
    let out = f(&repo);
    OPEN.with(|open| open.borrow_mut().insert(id, repo));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{bare_repo, fixture_repo};

    #[test]
    fn roots_are_found_from_inside_and_opened() {
        let (dir, _) = fixture_repo();
        let sub = dir.path().join("deep/er");
        std::fs::create_dir_all(&sub).unwrap();
        let root = RepoHandle::find_root(&sub).unwrap();
        assert_eq!(root, std::fs::canonicalize(dir.path()).unwrap());
        let handle = RepoHandle::open(root.clone()).unwrap();
        assert_eq!(handle.root(), root);
        assert!(!handle.is_bare());
        assert!(handle.git_dir().ends_with(".git"));

        let (_bare_dir, bare) = bare_repo();
        let bare_root = RepoHandle::find_root(bare.git_dir()).unwrap();
        assert_eq!(bare_root, std::fs::canonicalize(bare.git_dir()).unwrap());
        assert!(RepoHandle::open(bare_root).unwrap().is_bare());

        let elsewhere = tempfile::tempdir().unwrap();
        let err = RepoHandle::find_root(elsewhere.path()).unwrap_err();
        assert!(err.to_string().contains("no git repository found"), "{err}");
        assert!(RepoHandle::open(elsewhere.path().to_owned()).is_err());
    }
}
