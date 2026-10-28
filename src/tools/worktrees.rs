use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use gix::refs::{
    Target,
    transaction::{PreviousValue, RefEdit},
};

use super::{
    page_limit, paginate, resolve_commit, root_relative,
    write::{branch_ref, checked_out_at, committer},
};

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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorktreeAddArgs {
    /// Where to create the checkout: a path that doesn't exist yet (or an empty directory), absolute or
    /// relative to the repository root, e.g. `../myrepo-feature`.
    pub path: String,
    /// Branch to check out there, e.g. `feature`. With `create`, the new branch's name; with `detach`,
    /// any revision.
    pub target: String,
    /// Create `target` as a new branch at `start_point` first, like `git worktree add -b` (default false).
    #[serde(default)]
    pub create: bool,
    /// Where a new branch starts (default `HEAD`). Only with `create`.
    pub start_point: Option<String>,
    /// Check out `target` with a detached HEAD, like `git worktree add --detach` (default false).
    #[serde(default)]
    pub detach: bool,
}

/// Add a linked worktree with its own checkout, like `git worktree add`.
pub fn worktree_add(
    repo: &gix::Repository,
    args: WorktreeAddArgs,
) -> anyhow::Result<WorktreeEntry> {
    use anyhow::{Context, bail};
    use gix::worktree::add::Head;
    if args.create && args.detach {
        bail!("`create` and `detach` can't be combined");
    }
    if args.start_point.is_some() && !args.create {
        bail!("`start_point` only applies with `create: true`");
    }
    committer(repo)?;
    let head = if args.detach {
        Head::Detached(resolve_commit(repo, Some(&args.target))?.id)
    } else {
        let name = branch_ref(&args.target)?;
        let exists = repo.try_find_reference(name.as_bstr())?.is_some();
        match (args.create, exists) {
            (true, true) => bail!(
                "branch `{}` already exists; use it without `create`",
                args.target
            ),
            (false, false) => bail!(
                "there is no branch `{}`; set `create: true` to create it, or `detach: true` for a tag or commit",
                args.target
            ),
            _ => {}
        }
        if let Some(path) = checked_out_at(repo, name.as_ref(), false)? {
            bail!(
                "branch `{}` is already checked out at {}",
                args.target,
                path.display()
            );
        }
        Head::Attached(name)
    };
    let destination = root_relative(repo, &args.path);
    let destination = destination.as_path();
    if std::fs::read_dir(destination).is_ok_and(|mut entries| entries.next().is_some()) {
        bail!("`{}` exists and isn't empty; pick a new path", args.path);
    }
    let created = match (&head, args.create) {
        (Head::Attached(name), true) => {
            let start = args.start_point.as_deref().unwrap_or("HEAD");
            let id = resolve_commit(repo, Some(start))?.id;
            repo.edit_reference(RefEdit::update(
                name.clone(),
                id,
                PreviousValue::MustNotExist,
                format!("branch: Created from {start}"),
            ))?;
            Some((name.clone(), id))
        }
        _ => None,
    };
    let added = repo.add_worktree(
        destination,
        head,
        gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
    );
    if let Err(err) = added {
        // Don't leave behind a branch made only for this worktree.
        if let Some((name, id)) = created {
            repo.edit_reference(RefEdit::delete(
                name,
                PreviousValue::MustExistAndMatch(Target::Object(id)),
            ))
            .ok();
        }
        return Err(
            anyhow::Error::from(err).context(format!("cannot add a worktree at `{}`", args.path))
        );
    }
    let wanted = canonical(destination);
    let listed = list_worktrees(
        repo,
        ListWorktreesArgs {
            limit: Some(MAX_WORKTREE_LIMIT),
            skip: None,
        },
    )?;
    listed
        .worktrees
        .into_iter()
        .find(|w| {
            w.path
                .as_deref()
                .is_some_and(|p| canonical(Path::new(p)) == wanted)
        })
        .context("the worktree was added but isn't listed; check with list_worktrees")
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorktreeRemoveArgs {
    /// The linked worktree: its path, its `id` from `list_worktrees`, or a unique end of its path.
    pub worktree: String,
    /// Remove it even with uncommitted or untracked changes, or when locked, like
    /// `git worktree remove --force --force` (default false).
    #[serde(default)]
    pub force: bool,
    /// Only report what would be removed (default true). Set `false` to delete the checkout.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorktreeRemoveResult {
    /// Absolute path of the checkout.
    pub path: String,
    /// True when it has uncommitted or untracked changes, which removal deletes (needs `force`).
    pub dirty: bool,
    /// True when it's locked (needs `force`).
    pub locked: bool,
    /// True when the worktree was removed; false on a dry run. Its branch is kept.
    pub removed: bool,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// Remove a linked worktree's checkout and registration, like `git worktree remove`. Its branch is
/// kept. Dry run by default.
pub fn worktree_remove(
    repo: &gix::Repository,
    args: WorktreeRemoveArgs,
) -> anyhow::Result<WorktreeRemoveResult> {
    use anyhow::{Context, bail};
    let by_id = repo
        .worktree_proxy_by_id(gix::bstr::BStr::new(args.worktree.as_bytes()))
        .ok()
        .flatten();
    let path = match by_id {
        Some(proxy) => proxy.base()?,
        None => root_relative(repo, &args.worktree),
    };
    let target = repo.prepare_remove_worktree(&path).with_context(|| {
        format!(
            "no linked worktree matches `{}`; list_worktrees shows them",
            args.worktree
        )
    })?;
    let base = target.base().to_owned();
    let linked = target.repository()?;
    if canonical(linked.git_dir()) == canonical(repo.git_dir()) {
        bail!(
            "`{}` is the worktree this server serves; remove it from another worktree",
            args.worktree
        );
    }
    let locked = linked.git_dir().join("locked").exists();
    let dirty = base.is_dir() && linked.is_dirty()?;
    if (dirty || locked) && !args.force {
        let why = if locked {
            "locked"
        } else {
            "has uncommitted or untracked changes"
        };
        bail!(
            "the worktree at {} is {why}; set `force: true` to remove it anyway",
            base.display()
        );
    }
    if !args.dry_run {
        let force = if args.force {
            gix::worktree::remove::Force::OverrideLock
        } else {
            gix::worktree::remove::Force::Never
        };
        target.remove(force, gix::progress::Discard)?;
    }
    Ok(WorktreeRemoveResult {
        path: base.display().to_string(),
        dirty,
        locked,
        removed: !args.dry_run,
        dry_run: args.dry_run,
    })
}

/// Paths are compared after resolving symlinks such as macOS's `/tmp` → `/private/tmp`.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        assert_unchanged, bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git, snapshot,
    };

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

    fn add(
        repo: &gix::Repository,
        path: &Path,
        target: &str,
        create: bool,
        detach: bool,
    ) -> anyhow::Result<WorktreeEntry> {
        worktree_add(
            repo,
            WorktreeAddArgs {
                path: path.display().to_string(),
                target: target.into(),
                create,
                start_point: None,
                detach,
            },
        )
    }

    fn remove(
        repo: &gix::Repository,
        which: &str,
        force: bool,
        dry_run: bool,
    ) -> anyhow::Result<WorktreeRemoveResult> {
        worktree_remove(
            repo,
            WorktreeRemoveArgs {
                worktree: which.into(),
                force,
                dry_run,
            },
        )
    }

    #[test]
    fn added_worktrees_look_like_gits() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let parent = tempfile::tempdir().unwrap();
        let ours = parent.path().join("ours");
        let theirs = parent.path().join("theirs");
        let entry = add(&repo, &ours, "feature", false, false).unwrap();
        assert_eq!(entry.branch.as_deref(), Some("feature"));
        assert!(!entry.is_main && !entry.is_current);
        run_git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "topic",
                theirs.to_str().unwrap(),
            ],
        );
        assert_eq!(
            crate::tools::testutil::worktree_files(&ours),
            crate::tools::testutil::worktree_files(&theirs)
        );
        assert_eq!(git_stdout(&ours, &["status", "--porcelain"]), "");
        assert_eq!(rows(&list(&open(root))), git_worktrees(root));

        let created = add(&repo, &parent.path().join("new"), "fresh", true, false).unwrap();
        assert_eq!(created.branch.as_deref(), Some("fresh"));
        let detached = add(&repo, &parent.path().join("detached"), "v1", false, true).unwrap();
        assert!(detached.detached);
        assert_eq!(rows(&list(&open(root))), git_worktrees(root));
    }

    #[test]
    fn add_refusals_change_nothing() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let parent = tempfile::tempdir().unwrap();
        let refused = |needle: &str, result: anyhow::Result<WorktreeEntry>| {
            let err = format!("{:#}", result.unwrap_err());
            assert!(err.contains(needle), "expected `{needle}` in: {err}");
        };
        let before = snapshot(root);
        refused(
            "checked out",
            add(&repo, &parent.path().join("a"), "main", false, false),
        );
        refused(
            "no branch `nope`",
            add(&repo, &parent.path().join("b"), "nope", false, false),
        );
        refused(
            "already exists",
            add(&repo, &parent.path().join("c"), "feature", true, false),
        );
        crate::tools::testutil::write(&parent.path().join("full"), "x.txt", "occupied\n");
        refused(
            "isn't empty",
            add(&repo, &parent.path().join("full"), "feature", false, false),
        );
        assert_unchanged(root, &before);
        assert!(!parent.path().join("a").exists());
    }

    #[test]
    fn relative_paths_are_from_the_repository_root() {
        let (dir, repo) = fixture_repo();
        let entry = worktree_add(
            &repo,
            WorktreeAddArgs {
                path: "wt/linked".into(),
                target: "feature".into(),
                create: false,
                start_point: None,
                detach: false,
            },
        )
        .unwrap();
        let expected = std::fs::canonicalize(dir.path().join("wt/linked")).unwrap();
        assert_eq!(entry.path.as_deref(), Some(expected.to_str().unwrap()));
        let removed = remove(&repo, "wt/linked", false, false).unwrap();
        assert!(removed.removed);
        assert!(!dir.path().join("wt/linked").exists());
    }

    #[test]
    fn removal_matches_git_and_guards_changes() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("linked");
        add(&repo, &path, "feature", false, false).unwrap();

        let before = snapshot(root);
        let dry = remove(&repo, "linked", false, true).unwrap();
        assert!(dry.dry_run && !dry.removed && !dry.dirty);
        assert_unchanged(root, &before);
        assert!(path.exists());

        crate::tools::testutil::write(&path, "a.txt", "uncommitted\n");
        let err = remove(&repo, "linked", false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("uncommitted"), "{err}");
        run_git(root, &["worktree", "lock", path.to_str().unwrap()]);
        let err = remove(&repo, path.to_str().unwrap(), false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("locked"), "{err}");

        let done = remove(&repo, "linked", true, false).unwrap();
        assert!(done.removed && done.dirty && done.locked);
        assert!(!path.exists());
        assert_eq!(list(&open(root)).worktrees.len(), 1);
        assert!(
            !git_stdout(root, &["rev-parse", "--verify", "feature"]).is_empty(),
            "the branch stays"
        );

        let err = remove(&repo, "nope", false, true).unwrap_err().to_string();
        assert!(err.contains("no linked worktree"), "{err}");
        let linked = parent.path().join("served");
        add(&repo, &linked, "fresh", true, false).unwrap();
        let err = remove(&open(&linked), "served", false, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("this server serves"), "{err}");
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
