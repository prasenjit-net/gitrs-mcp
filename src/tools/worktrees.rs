use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{page_limit, paginate};

const DEFAULT_WORKTREE_LIMIT: usize = 100;
const MAX_WORKTREE_LIMIT: usize = 1000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListWorktreesArgs {
    /// Maximum worktrees to return (default 100, max 1000).
    pub limit: Option<usize>,
    /// Number of worktrees to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorktreeEntry {
    /// Absolute path of the checkout (for a bare main repository, the repository itself); `null` when a
    /// linked worktree's registration can't be read, which also makes it `prunable`.
    pub path: Option<String>,
    /// Name of a linked worktree: its directory under `.git/worktrees/`. `null` for the main worktree.
    pub id: Option<String>,
    /// True for the main worktree, the one the repository was created or cloned with.
    pub is_main: bool,
    /// True when the main repository is bare, so it has no checkout of its own.
    pub is_bare: bool,
    /// True for the worktree this server serves.
    pub is_current: bool,
    /// Full SHA the worktree's HEAD points to; `null` when its branch has no commits yet or HEAD can't be read.
    pub head: Option<String>,
    /// Short name of the branch checked out, e.g. `main`; `null` when HEAD is detached.
    pub branch: Option<String>,
    /// True when the worktree's HEAD is detached.
    pub detached: bool,
    /// True when the worktree is locked (`git worktree lock`), so it won't be pruned, moved or removed.
    pub locked: bool,
    /// Reason given when locking; `null` when unlocked or no reason was given.
    pub lock_reason: Option<String>,
    /// True when the checkout directory is gone, so `git worktree prune` would remove the registration.
    pub prunable: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListWorktreesResult {
    /// The main worktree first, then linked worktrees ordered by `id`, like `git worktree list`.
    pub worktrees: Vec<WorktreeEntry>,
    /// True if more worktrees exist past this page.
    pub truncated: bool,
}

/// The main worktree and every linked worktree, like `git worktree list`.
pub fn list_worktrees(
    repo: &gix::Repository,
    args: ListWorktreesArgs,
) -> anyhow::Result<ListWorktreesResult> {
    let current = canonical(repo.git_dir());
    let main = repo.main_repo()?;
    let mut worktrees = vec![main_entry(&main, &current)];
    for proxy in repo.worktrees()? {
        worktrees.push(linked_entry(proxy, &current)?);
    }
    let limit = page_limit(args.limit, DEFAULT_WORKTREE_LIMIT, MAX_WORKTREE_LIMIT);
    let (worktrees, truncated) = paginate(
        worktrees.into_iter().map(anyhow::Ok),
        args.skip.unwrap_or(0),
        limit,
    )?;
    Ok(ListWorktreesResult {
        worktrees,
        truncated,
    })
}

fn main_entry(main: &gix::Repository, current: &Path) -> WorktreeEntry {
    let head = HeadState::of(main);
    WorktreeEntry {
        path: Some(
            main.workdir()
                .unwrap_or_else(|| main.git_dir())
                .display()
                .to_string(),
        ),
        id: None,
        is_main: true,
        is_bare: main.is_bare(),
        is_current: canonical(main.git_dir()) == current,
        head: head.id,
        branch: head.branch,
        detached: head.detached,
        locked: false,
        lock_reason: None,
        prunable: false,
    }
}

fn linked_entry(proxy: gix::worktree::Proxy<'_>, current: &Path) -> anyhow::Result<WorktreeEntry> {
    let id = proxy.id()?.to_string();
    let path = proxy.base().ok().map(|p| p.display().to_string());
    let locked = proxy.is_locked();
    let lock_reason = proxy
        .lock_reason()
        .filter(|reason| !reason.is_empty())
        .map(|reason| reason.to_string());
    let prunable = proxy.is_prunable();
    let is_current = canonical(proxy.git_dir()) == current;
    let head = proxy
        .into_repo_with_possibly_inaccessible_worktree()
        .map(|repo| HeadState::of(&repo))
        .unwrap_or_default();
    Ok(WorktreeEntry {
        path,
        id: Some(id),
        is_main: false,
        is_bare: false,
        is_current,
        head: head.id,
        branch: head.branch,
        detached: head.detached,
        locked,
        lock_reason,
        prunable,
    })
}

/// What a worktree's HEAD points at.
#[derive(Default)]
struct HeadState {
    id: Option<String>,
    branch: Option<String>,
    detached: bool,
}

impl HeadState {
    /// A worktree whose HEAD can't be read is still listed, as `git worktree list` does, just without it.
    fn of(repo: &gix::Repository) -> Self {
        match repo.head() {
            Ok(head) => Self {
                id: head.id().map(|id| id.to_string()),
                branch: head.referent_name().map(|name| name.shorten().to_string()),
                detached: head.is_detached(),
            },
            Err(err) => {
                tracing::debug!(%err, git_dir = %repo.git_dir().display(), "cannot read worktree HEAD");
                Self::default()
            }
        }
    }
}

/// Paths are compared after resolving symlinks such as macOS's `/tmp` → `/private/tmp`.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git};

    fn list(repo: &gix::Repository) -> ListWorktreesResult {
        list_worktrees(
            repo,
            ListWorktreesArgs {
                limit: None,
                skip: None,
            },
        )
        .unwrap()
    }

    /// A worktree as `git worktree list --porcelain` describes it: `(path, head, branch, other lines)`.
    type Row = (String, Option<String>, Option<String>, Vec<String>);

    /// One record per worktree from `git worktree list --porcelain`.
    fn git_worktrees(dir: &Path) -> Vec<Row> {
        git_stdout(dir, &["worktree", "list", "--porcelain"])
            .split("\n\n")
            .filter(|block| !block.trim().is_empty())
            .map(|block| {
                let mut path = String::new();
                let (mut head, mut branch, mut flags) = (None, None, Vec::new());
                for line in block.lines() {
                    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
                    match word {
                        "worktree" => path = rest.to_owned(),
                        "HEAD" => head = Some(rest.to_owned()),
                        "branch" => {
                            branch = Some(rest.trim_start_matches("refs/heads/").to_owned())
                        }
                        _ => flags.push(line.to_owned()),
                    }
                }
                (path, head, branch, flags)
            })
            .collect()
    }

    fn rows(res: &ListWorktreesResult) -> Vec<Row> {
        res.worktrees
            .iter()
            .map(|w| {
                let mut flags = Vec::new();
                if w.is_bare {
                    flags.push("bare".to_owned());
                }
                if w.detached {
                    flags.push("detached".to_owned());
                }
                if w.locked {
                    flags.push(match &w.lock_reason {
                        Some(reason) => format!("locked {reason}"),
                        None => "locked".to_owned(),
                    });
                }
                if w.prunable {
                    flags.push("prunable gitdir file points to non-existent location".to_owned());
                }
                let path = canonical(Path::new(w.path.as_deref().unwrap()));
                // git prints neither HEAD nor the branch for a bare repository; we report both.
                let (head, branch) = if w.is_bare {
                    (None, None)
                } else {
                    (w.head.clone(), w.branch.clone())
                };
                (path.display().to_string(), head, branch, flags)
            })
            .collect()
    }

    #[test]
    fn linked_worktrees_match_git() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let parent = tempfile::tempdir().unwrap();
        let at = |name: &str| parent.path().join(name).display().to_string();
        run_git(
            root,
            &["worktree", "add", "-q", &at("zz-feature"), "feature"],
        );
        run_git(
            root,
            &["worktree", "add", "-q", "--detach", &at("aa-detached")],
        );
        run_git(root, &["worktree", "add", "-q", "-b", "temp", &at("gone")]);
        std::fs::remove_dir_all(parent.path().join("gone")).unwrap();
        run_git(
            root,
            &["worktree", "lock", "--reason", "on usb", &at("zz-feature")],
        );
        let repo = open(root);

        let res = list(&repo);
        assert_eq!(rows(&res), git_worktrees(root));
        let ids: Vec<_> = res.worktrees.iter().map(|w| w.id.as_deref()).collect();
        assert_eq!(
            ids,
            [None, Some("aa-detached"), Some("gone"), Some("zz-feature")]
        );
        assert!(res.worktrees[0].is_main && res.worktrees[0].is_current);
        assert!(
            res.worktrees[1..]
                .iter()
                .all(|w| !w.is_main && !w.is_current)
        );
        assert!(!res.truncated);

        // Served from a linked worktree, that one is current and the main one is still listed first.
        let linked = open(&parent.path().join("aa-detached"));
        let from_linked = list(&linked);
        assert_eq!(rows(&from_linked), rows(&res));
        assert!(from_linked.worktrees[1].is_current && !from_linked.worktrees[0].is_current);

        let page = list_worktrees(
            &repo,
            ListWorktreesArgs {
                limit: Some(2),
                skip: Some(1),
            },
        )
        .unwrap();
        assert_eq!(page.worktrees.len(), 2);
        assert_eq!(page.worktrees[0].id.as_deref(), Some("aa-detached"));
        assert!(page.truncated);
    }

    #[test]
    fn single_bare_and_empty_repositories() {
        let (dir, repo) = fixture_repo();
        let res = list(&repo);
        assert_eq!(rows(&res), git_worktrees(dir.path()));
        assert_eq!(res.worktrees[0].branch.as_deref(), Some("main"));

        let (dir, bare) = bare_repo();
        let res = list(&bare);
        assert!(res.worktrees[0].is_bare);
        assert_eq!(res.worktrees[0].branch.as_deref(), Some("main"));
        assert!(res.worktrees[0].head.is_some());
        assert_eq!(rows(&res), git_worktrees(&dir.path().join("bare.git")));

        let (_dir, empty) = empty_repo();
        let res = list(&empty);
        assert_eq!(res.worktrees[0].head, None);
        assert_eq!(res.worktrees[0].branch.as_deref(), Some("main"));
    }
}
