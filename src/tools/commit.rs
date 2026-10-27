use std::path::PathBuf;

use anyhow::{Context, bail};
use gix::{
    ObjectId,
    index::entry::{Flags, Stage},
    refs::{
        Target,
        transaction::{PreviousValue, RefEdit},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    LogEntry,
    diff::index_changes,
    index_entry_kind, resolve_commit,
    staging::reset_entries,
    write::{
        clean_message, committer, ensure_idle, hold_index_lock, refuse_signing, require_worktree,
        update_index,
    },
};

/// The hooks `git commit` runs, which gitrs-mcp doesn't.
const COMMIT_HOOKS: [&str; 4] = [
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Person {
    /// Full name, e.g. `Ada Lovelace`.
    pub name: String,
    /// Email address, e.g. `ada@example.com`.
    pub email: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommitArgs {
    /// Commit message: a summary line, optionally a blank line and a body. Whitespace is tidied as
    /// `git commit -m` does.
    pub message: String,
    /// Replace the current commit instead of adding a new one, like `git commit --amend` (default
    /// false). The original author is kept unless `author` is given.
    #[serde(default)]
    pub amend: bool,
    /// Create the commit even if it records no change from its parent (default false).
    #[serde(default)]
    pub allow_empty: bool,
    /// Author to record instead of the configured identity, e.g. when committing someone else's
    /// change. The committer is always the configured identity.
    pub author: Option<Person>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitResult {
    /// The new commit, as `log` lists it (flattened into this object).
    #[serde(flatten)]
    pub commit: LogEntry,
    /// Full SHA of the commit's tree.
    pub tree: String,
    /// Short name of the branch that now points to the commit; `null` when HEAD is detached.
    pub branch: Option<String>,
    /// With `amend`: full SHA of the commit that was replaced. It stays reachable through the reflog.
    pub amended: Option<String>,
    /// Hooks in the repository that `git commit` would have run (`pre-commit`, `commit-msg`, ...).
    /// gitrs-mcp doesn't run hooks, so their checks were skipped.
    pub hooks_skipped: Vec<String>,
}

/// Record the index as a new commit on the current branch, like `git commit -m`.
pub fn commit(repo: &gix::Repository, args: CommitArgs) -> anyhow::Result<CommitResult> {
    require_worktree(repo, "commit")?;
    ensure_idle(repo, "commit")?;
    refuse_signing(repo, "commit.gpgSign", "commits")?;
    let message = clean_message(&args.message)
        .context("`message` is empty; describe the change in at least a summary line")?;
    let committer = committer(repo)?;

    // Like `git commit`, keep the index locked while it's turned into a tree.
    let _lock = hold_index_lock(repo)?;
    let index = repo.index_or_empty()?;
    if index
        .entries()
        .iter()
        .any(|e| e.stage() != Stage::Unconflicted)
    {
        bail!(
            "the index has unresolved conflicts; resolve them and stage the files (`conflicts` lists them)"
        );
    }
    // Compute the tree in memory first, so a refusal below leaves the object database untouched.
    let tree = write_tree(&repo.clone().with_object_memory(), &index)?;

    let head = repo.head()?;
    let branch = head.referent_name().map(|name| name.shorten().to_string());
    let head_id = head.id().map(|id| id.detach());
    let previous = head_id.map(|id| repo.find_commit(id)).transpose()?;
    let (parents, original_author) = match (&previous, args.amend) {
        (None, true) => bail!("there is no commit to amend yet; commit without `amend`"),
        (Some(old), true) => (
            old.parent_ids().map(|id| id.detach()).collect::<Vec<_>>(),
            Some(old.author()?.to_owned()?),
        ),
        (Some(_), false) => (head_id.into_iter().collect(), None),
        (None, false) => (Vec::new(), None),
    };
    if !args.allow_empty {
        ensure_changes(repo, tree, parents.first().copied(), args.amend)?;
    }
    let author = match (args.author, original_author) {
        (Some(person), _) => person_signature(person, &committer)?,
        (None, Some(original)) => original,
        (None, None) => match repo.author() {
            Some(author) => author?.to_owned()?,
            None => committer.clone(),
        },
    };

    write_tree(repo, &index)?;
    let commit = gix::objs::Commit {
        tree,
        parents: parents.into_iter().collect(),
        author,
        committer,
        encoding: None,
        message: message.clone().into(),
        extra_headers: Vec::new(),
    };
    let id = repo.write_object(&commit)?.detach();
    let summary = message.lines().next().unwrap_or_default();
    let log_message = if args.amend {
        format!("commit (amend): {summary}")
    } else {
        gix::reference::log::message("commit", summary.into(), commit.parents.len()).to_string()
    };
    let expected = match head_id {
        Some(old) => PreviousValue::MustExistAndMatch(Target::Object(old)),
        None => PreviousValue::MustNotExist,
    };
    repo.edit_reference(
        RefEdit::update("HEAD".try_into()?, id, expected, log_message).with_deref(true),
    )?;

    Ok(CommitResult {
        commit: LogEntry::from_commit(&repo.find_commit(id)?)?,
        tree: tree.to_string(),
        branch,
        amended: args
            .amend
            .then(|| head_id.map(|id| id.to_string()))
            .flatten(),
        hooks_skipped: present_hooks(repo),
    })
}

/// The tree the index describes, written to the object database. Intent-to-add entries are left
/// out, as `git commit` does.
fn write_tree(repo: &gix::Repository, index: &gix::index::State) -> anyhow::Result<ObjectId> {
    let mut editor = repo.edit_tree(ObjectId::empty_tree(repo.object_hash()))?;
    for entry in index.entries() {
        if entry.flags.contains(Flags::INTENT_TO_ADD) {
            continue;
        }
        editor.upsert(entry.path(index), index_entry_kind(entry.mode), entry.id)?;
    }
    Ok(editor.write()?.detach())
}

/// Refuse a commit whose tree equals its first parent's, as git does without `--allow-empty`.
fn ensure_changes(
    repo: &gix::Repository,
    tree: ObjectId,
    parent: Option<ObjectId>,
    amend: bool,
) -> anyhow::Result<()> {
    let parent_tree = match parent {
        Some(parent) => repo.find_commit(parent)?.tree_id()?.detach(),
        None => ObjectId::empty_tree(repo.object_hash()),
    };
    if tree != parent_tree {
        return Ok(());
    }
    if amend {
        bail!(
            "amending would leave the commit with no changes from its parent; set `allow_empty: true` \
             to do it anyway"
        );
    }
    bail!(
        "nothing to commit: the index matches HEAD. Stage changes first (`status` shows them), or set \
         `allow_empty: true`"
    );
}

fn person_signature(
    person: Person,
    committer: &gix::actor::Signature,
) -> anyhow::Result<gix::actor::Signature> {
    let invalid = |s: &str| s.trim().is_empty() || s.contains(['<', '>', '\n']);
    if invalid(&person.name) || invalid(&person.email) {
        bail!("`author` needs a non-empty name and email without `<`, `>` or line breaks");
    }
    Ok(gix::actor::Signature {
        name: person.name.trim().into(),
        email: person.email.trim().into(),
        time: committer.time,
    })
}

/// Commit hooks that exist and are executable, which `git commit` would have run.
fn present_hooks(repo: &gix::Repository) -> Vec<String> {
    let dir = repo
        .config_snapshot()
        .trusted_path("core.hooksPath")
        .ok()
        .flatten()
        .map(|path: PathBuf| match repo.workdir() {
            Some(workdir) if path.is_relative() => workdir.join(path),
            _ => path,
        })
        .unwrap_or_else(|| repo.common_dir().join("hooks"));
    COMMIT_HOOKS
        .iter()
        .filter(|hook| is_executable_file(&dir.join(hook)))
        .map(|hook| hook.to_string())
        .collect()
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

/// How far `reset` goes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResetMode {
    /// Move the branch only; the index and working tree keep their content, so the undone commits'
    /// changes show up as staged.
    Soft,
    /// Move the branch and reset the index to the target commit; the working tree keeps its content,
    /// so the changes show up as unstaged (the default, like `git reset`).
    #[default]
    Mixed,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResetArgs {
    /// Commit to move the current branch (or detached HEAD) to, e.g. `HEAD~1` or a SHA (default `HEAD`,
    /// which with `mixed` just unstages everything).
    pub rev: Option<String>,
    /// `soft` or `mixed` (default). `hard`, which also overwrites the working tree, isn't available.
    #[serde(default)]
    pub mode: ResetMode,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ResetResult {
    /// The mode that was applied.
    pub mode: ResetMode,
    /// Full SHA HEAD pointed to before. To undo, reset to it; it also stays in the reflog.
    pub previous: String,
    /// Full SHA HEAD points to now.
    pub head: String,
    /// Short name of the branch that moved; `null` when HEAD is detached.
    pub branch: Option<String>,
    /// Number of paths that now differ between HEAD and the index (staged changes).
    pub staged_total: usize,
}

/// Move the current branch to another commit, like `git reset --soft` or `--mixed`. The working
/// tree is never touched.
pub fn reset(repo: &gix::Repository, args: ResetArgs) -> anyhow::Result<ResetResult> {
    ensure_idle(repo, "reset")?;
    let rev = args.rev.as_deref().unwrap_or("HEAD");
    let previous = repo
        .head_id()
        .ok()
        .context("HEAD has no commits yet, so there is nothing to reset")?
        .detach();
    let target = resolve_commit(repo, Some(rev))?;
    let target_tree = target.tree_id()?.detach();
    let target = target.id;
    let branch = repo.head_name()?.map(|name| name.shorten().to_string());
    let move_head = || -> anyhow::Result<()> {
        repo.edit_reference(
            RefEdit::update(
                "HEAD".try_into()?,
                target,
                PreviousValue::MustExistAndMatch(Target::Object(previous)),
                format!("reset: moving to {rev}"),
            )
            .with_deref(true),
        )?;
        Ok(())
    };
    if args.mode == ResetMode::Mixed {
        require_worktree(repo, "reset the index")?;
    }
    committer(repo)?;
    match args.mode {
        ResetMode::Soft => {
            let index = repo.index_or_empty()?;
            if index
                .entries()
                .iter()
                .any(|e| e.stage() != Stage::Unconflicted)
            {
                bail!(
                    "the index has unresolved conflicts, which a soft reset would keep; use `mixed`"
                );
            }
            move_head()?;
        }
        ResetMode::Mixed => {
            // Move HEAD inside the index update: if the ref can't be moved, the index stays as it was.
            update_index(repo, |index| {
                reset_entries(repo, index, target_tree, &[])?;
                move_head()
            })?;
        }
    }
    let (staged, _) = index_changes(repo, &target_tree, &[])?;
    Ok(ResetResult {
        mode: args.mode,
        previous: previous.to_string(),
        head: target.to_string(),
        branch,
        staged_total: staged.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{
        assert_unchanged, bare_repo, empty_repo, fixture_repo, git_stdout, ls_files, open,
        porcelain, run_git, run_git_with_env, snapshot, try_git, twins, write,
    };

    fn commit_msg(repo: &gix::Repository, message: &str) -> anyhow::Result<CommitResult> {
        commit(
            repo,
            CommitArgs {
                message: message.into(),
                amend: false,
                allow_empty: false,
                author: None,
            },
        )
    }

    fn rev(root: &Path, spec: &str) -> String {
        git_stdout(root, &["rev-parse", spec]).trim().to_owned()
    }

    fn last_reflog(root: &Path, name: &str) -> String {
        git_stdout(root, &["reflog", "show", "-1", "--format=%gs", name])
            .trim()
            .to_owned()
    }

    fn refused<T: std::fmt::Debug>(
        root: &Path,
        needle: &str,
        f: impl FnOnce() -> anyhow::Result<T>,
    ) {
        let before = snapshot(root);
        let err = format!("{:#}", f().unwrap_err());
        assert!(err.contains(needle), "expected `{needle}` in: {err}");
        assert_unchanged(root, &before);
    }

    /// Staged edits including an intent-to-add file, which commits leave out.
    fn staged(root: &Path) {
        write(root, "a.txt", "one\ntwo\nthree\n");
        write(root, "src/new.rs", "fn new() {}\n");
        write(root, "later.txt", "not yet\n");
        run_git(root, &["add", "a.txt", "src/new.rs"]);
        run_git(root, &["add", "-N", "later.txt"]);
    }

    #[test]
    fn commit_matches_git_commit() {
        let [(ours, repo), (theirs, _)] = twins(staged);
        let before = rev(ours.path(), "HEAD");
        let res = commit_msg(&repo, "  Add files  \n\n\n\nWith a body.  \n").unwrap();
        run_git(
            theirs.path(),
            &[
                "commit",
                "-q",
                "-m",
                "  Add files  \n\n\n\nWith a body.  \n",
            ],
        );

        let root = ours.path();
        assert_eq!(rev(root, "HEAD"), res.commit.id);
        assert_eq!(rev(root, "HEAD^{tree}"), rev(theirs.path(), "HEAD^{tree}"));
        assert_eq!(res.tree, rev(root, "HEAD^{tree}"));
        assert_eq!(res.commit.parents, [before]);
        assert_eq!(res.branch.as_deref(), Some("main"));
        assert_eq!(
            git_stdout(root, &["log", "-1", "--format=%B"]),
            git_stdout(theirs.path(), &["log", "-1", "--format=%B"]),
            "the message is cleaned up the same way"
        );
        assert_eq!(
            git_stdout(root, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]).trim(),
            "Test <test@example.com>|Test <test@example.com>"
        );
        assert_eq!(
            last_reflog(root, "HEAD"),
            last_reflog(theirs.path(), "HEAD")
        );
        assert_eq!(last_reflog(root, "main"), "commit: Add files");
        assert_eq!(
            porcelain(root),
            porcelain(theirs.path()),
            "the intent-to-add file stays out"
        );
        assert!(try_git(root, &["fsck", "--strict"]));
        assert!(res.amended.is_none() && res.hooks_skipped.is_empty());
    }

    #[test]
    fn initial_detached_and_empty_commits() {
        let (dir, repo) = empty_repo();
        let root = dir.path();
        write(root, "first.txt", "hello\n");
        run_git(root, &["add", "."]);
        let first = commit_msg(&repo, "First").unwrap();
        assert!(first.commit.parents.is_empty());
        assert_eq!(last_reflog(root, "HEAD"), "commit (initial): First");

        refused(root, "nothing to commit", || commit_msg(&repo, "Again"));
        let empty = commit(
            &repo,
            CommitArgs {
                message: "Empty".into(),
                amend: false,
                allow_empty: true,
                author: None,
            },
        )
        .unwrap();
        assert_eq!(empty.tree, first.tree);

        run_git(root, &["checkout", "-q", "--detach"]);
        write(root, "first.txt", "detached\n");
        run_git(root, &["add", "."]);
        let detached = commit_msg(&repo, "On a detached HEAD").unwrap();
        assert!(detached.branch.is_none());
        assert_eq!(rev(root, "HEAD"), detached.commit.id);
        assert_eq!(rev(root, "main"), empty.commit.id, "no branch moved");
    }

    #[test]
    fn amend_keeps_the_author_and_parents() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "a.txt", "by someone else\n");
        run_git_with_env(
            root,
            &["commit", "-q", "-am", "Their change"],
            &[
                ("GIT_AUTHOR_NAME", "Other"),
                ("GIT_AUTHOR_EMAIL", "other@example.com"),
            ],
        );
        let old = rev(root, "HEAD");
        let parent = rev(root, "HEAD~1");
        let repo = open(root);
        let res = commit(
            &repo,
            CommitArgs {
                message: "Their change, reworded".into(),
                amend: true,
                allow_empty: false,
                author: None,
            },
        )
        .unwrap();
        assert_eq!(res.amended.as_deref(), Some(old.as_str()));
        assert_eq!(res.commit.parents, [parent]);
        assert_eq!(res.commit.author.name, "Other");
        assert_eq!(
            git_stdout(root, &["log", "-1", "--format=%cn"]).trim(),
            "Test"
        );
        assert_eq!(
            last_reflog(root, "HEAD"),
            "commit (amend): Their change, reworded"
        );

        let custom = commit(
            &repo,
            CommitArgs {
                message: "Credit".into(),
                amend: true,
                allow_empty: false,
                author: Some(Person {
                    name: "Grace Hopper".into(),
                    email: "grace@example.com".into(),
                }),
            },
        )
        .unwrap();
        assert_eq!(custom.commit.author.email, "grace@example.com");

        // Undoing the commit's only change by amending needs allow_empty.
        run_git(root, &["checkout", "-q", "HEAD~1", "--", "a.txt"]);
        refused(root, "allow_empty", || {
            commit(
                &repo,
                CommitArgs {
                    message: "Nothing left".into(),
                    amend: true,
                    allow_empty: false,
                    author: None,
                },
            )
        });
    }

    #[test]
    fn commit_refusals_change_nothing() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        write(root, "a.txt", "staged\n");
        run_git(root, &["add", "a.txt"]);
        refused(root, "message` is empty", || commit_msg(&repo, " \n\t"));
        refused(root, "author", || {
            commit(
                &repo,
                CommitArgs {
                    message: "x".into(),
                    amend: false,
                    allow_empty: false,
                    author: Some(Person {
                        name: "A <b>".into(),
                        email: "a@b".into(),
                    }),
                },
            )
        });
        std::fs::write(root.join(".git/index.lock"), "").unwrap();
        refused(root, "index.lock", || commit_msg(&repo, "Locked"));
        std::fs::remove_file(root.join(".git/index.lock")).unwrap();
        run_git(root, &["config", "commit.gpgSign", "true"]);
        refused(root, "can't sign commits", || {
            commit_msg(&open(root), "Signed?")
        });
        run_git(root, &["config", "commit.gpgSign", "false"]);

        let (_dir, empty) = empty_repo();
        let err = commit(
            &empty,
            CommitArgs {
                message: "x".into(),
                amend: true,
                allow_empty: true,
                author: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("no commit to amend"), "{err}");
        let (_dir, bare) = bare_repo();
        assert!(
            commit_msg(&bare, "x")
                .unwrap_err()
                .to_string()
                .contains("bare")
        );
    }

    #[test]
    fn commit_refuses_during_a_merge_and_reports_hooks() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        std::fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            root.join(".git/hooks/pre-commit"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        write(root, "a.txt", "hooked\n");
        run_git(root, &["add", "a.txt"]);
        let res = commit_msg(&open(root), "With a hook").unwrap();
        assert_eq!(res.hooks_skipped, ["pre-commit"]);

        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "--no-verify", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        refused(root, "in progress", || commit_msg(&open(root), "Mid-merge"));
    }

    fn reset_to(
        repo: &gix::Repository,
        rev: Option<&str>,
        mode: ResetMode,
    ) -> anyhow::Result<ResetResult> {
        reset(
            repo,
            ResetArgs {
                rev: rev.map(Into::into),
                mode,
            },
        )
    }

    #[test]
    fn soft_and_mixed_resets_match_git() {
        for (mode, flag) in [(ResetMode::Soft, "--soft"), (ResetMode::Mixed, "--mixed")] {
            let [(ours, repo), (theirs, _)] = twins(staged);
            let worktree = snapshot(&ours.path().join("src"));
            let previous = rev(ours.path(), "HEAD");
            let res = reset_to(&repo, Some("HEAD~1"), mode).unwrap();
            run_git(theirs.path(), &["reset", "-q", flag, "HEAD~1"]);
            assert_eq!(ls_files(ours.path()), ls_files(theirs.path()), "{flag}");
            assert_eq!(porcelain(ours.path()), porcelain(theirs.path()), "{flag}");
            assert_eq!(
                snapshot(&ours.path().join("src")),
                worktree,
                "the worktree is untouched"
            );
            assert_eq!(res.previous, previous);
            assert_eq!(res.head, rev(ours.path(), "HEAD"));
            assert_eq!(res.branch.as_deref(), Some("main"));
            assert_eq!(last_reflog(ours.path(), "main"), "reset: moving to HEAD~1");
            let staged = git_stdout(ours.path(), &["diff", "--cached", "--name-only"]);
            assert_eq!(res.staged_total, staged.lines().count(), "{flag}");
        }
    }

    #[test]
    fn mixed_reset_to_head_unstages_everything() {
        let [(ours, repo), (theirs, _)] = twins(staged);
        let parsed: ResetArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.mode, ResetMode::Mixed);
        let res = reset(&repo, parsed).unwrap();
        run_git(theirs.path(), &["reset", "-q"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(res.previous, res.head);
        assert_eq!(res.staged_total, 0);
    }

    #[test]
    fn reset_refusals_change_nothing() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        refused(root, "cannot resolve", || {
            reset_to(&repo, Some("nope"), ResetMode::Mixed)
        });
        let (dir, empty) = empty_repo();
        refused(dir.path(), "no commits yet", || {
            reset_to(&empty, None, ResetMode::Soft)
        });

        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "ours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        refused(root, "in progress", || {
            reset_to(&open(root), Some("HEAD~1"), ResetMode::Mixed)
        });

        let (dir, bare) = bare_repo();
        let root = dir.path().join("bare.git");
        refused(&root, "bare", || {
            reset_to(&bare, Some("HEAD~1"), ResetMode::Mixed)
        });
        let soft = reset_to(&bare, Some("HEAD~1"), ResetMode::Soft).unwrap();
        assert_eq!(
            soft.head,
            rev(&root, "HEAD"),
            "a soft reset works without a worktree"
        );
    }
}
