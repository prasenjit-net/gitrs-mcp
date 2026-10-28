use std::path::PathBuf;

use anyhow::{Context, bail};
use gix::{
    ObjectId,
    refs::{
        FullName, Target,
        transaction::{PreviousValue, RefEdit},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    refs::{ShowTagArgs, ShowTagResult, show_tag},
    resolve_commit, resolve_object,
    write::{
        branch_ref, checked_out_at, clean_message, committer, ensure_idle, log_head_move,
        refuse_signing, tag_ref,
    },
};

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateBranchArgs {
    /// Name of the new branch, e.g. `feature/login` (`refs/heads/` is added).
    pub name: String,
    /// Commit the branch starts at (default `HEAD`): a branch, tag, SHA or expression like `HEAD~2`.
    pub start_point: Option<String>,
    /// Move the branch if it already exists, like `git branch --force` (default false). Never allowed
    /// for a branch checked out in a worktree.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BranchResult {
    /// Full ref name, e.g. `refs/heads/feature/login`.
    pub name: String,
    /// Short name, e.g. `feature/login`.
    pub short_name: String,
    /// Full SHA the branch points to now.
    pub target: String,
    /// Full SHA the branch pointed to before a forced move; `null` when it was created.
    pub previous: Option<String>,
}

/// Create a branch at a commit, like `git branch <name> [<start>]`. HEAD doesn't move.
pub fn create_branch(
    repo: &gix::Repository,
    args: CreateBranchArgs,
) -> anyhow::Result<BranchResult> {
    let name = branch_ref(&args.name)?;
    let short = name.shorten().to_string();
    let start = args.start_point.as_deref().unwrap_or("HEAD");
    let target = resolve_commit(repo, Some(start))?.id;
    committer(repo)?;

    let previous = direct_target(repo, &name)?;
    let (expected, message) = match previous {
        None => (
            PreviousValue::MustNotExist,
            format!("branch: Created from {start}"),
        ),
        Some(old) if args.force => {
            if let Some(path) = checked_out_at(repo, name.as_ref(), false)? {
                bail!(
                    "branch `{short}` is checked out at {}; it can't be moved while checked out",
                    path.display()
                );
            }
            (
                PreviousValue::MustExistAndMatch(Target::Object(old)),
                format!("branch: Reset to {start}"),
            )
        }
        Some(old) => bail!(
            "branch `{short}` already exists at {old}; choose another name, or set `force: true` to \
             move it to `{start}`"
        ),
    };
    repo.edit_reference(RefEdit::update(name.clone(), target, expected, message))?;
    Ok(BranchResult {
        name: name.as_bstr().to_string(),
        short_name: short,
        target: target.to_string(),
        previous: previous.map(|id| id.to_string()),
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteBranchArgs {
    /// Branch to delete, e.g. `feature/login`.
    pub name: String,
    /// Delete even if its commits aren't merged, like `git branch -D` (default false).
    #[serde(default)]
    pub force: bool,
    /// Only check and report, without deleting (default true). Set `false` to delete.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeleteBranchResult {
    /// Full ref name, e.g. `refs/heads/feature/login`.
    pub name: String,
    /// Full SHA the branch pointed to. To undo, call `create_branch` with this as `start_point`.
    pub target: String,
    /// True when every commit on the branch is reachable from `merged_into`.
    pub merged: bool,
    /// What `merged` was checked against: the branch's upstream (e.g. `refs/remotes/origin/x`) when one
    /// is configured and present, otherwise `HEAD`, as `git branch -d` does.
    pub merged_into: String,
    /// True when the branch was deleted; false on a dry run.
    pub deleted: bool,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// Delete a local branch, like `git branch -d` (`-D` with `force`). Dry run by default.
pub fn delete_branch(
    repo: &gix::Repository,
    args: DeleteBranchArgs,
) -> anyhow::Result<DeleteBranchResult> {
    let name = branch_ref(&args.name)?;
    let short = name.shorten().to_string();
    let target = direct_target(repo, &name)?.with_context(|| {
        format!("there is no branch `{short}`; list_refs shows the branches that exist")
    })?;
    if let Some(path) = checked_out_at(repo, name.as_ref(), false)? {
        bail!(
            "branch `{short}` is checked out at {}; switch that worktree to another branch first",
            path.display()
        );
    }
    let (merged_into, base) = merge_check_base(repo, &name)?;
    let merged = match base {
        Some(base) => is_ancestor(repo, target, base)?,
        None => false,
    };
    if !merged && !args.force {
        bail!(
            "branch `{short}` is not fully merged into `{merged_into}`; deleting it could lose \
             commits. Check with compare_refs, or set `force: true` to delete it anyway"
        );
    }
    if !args.dry_run {
        committer(repo)?;
        // Also removes the branch's `branch.<name>.*` configuration, as git does.
        repo.clone()
            .delete_local_branches_if_unchanged([(name.clone(), Target::Object(target))])?;
    }
    Ok(DeleteBranchResult {
        name: name.as_bstr().to_string(),
        target: target.to_string(),
        merged,
        merged_into,
        deleted: !args.dry_run,
        dry_run: args.dry_run,
    })
}

/// The upstream tracking ref when it's configured and exists, otherwise `HEAD`; and its commit.
fn merge_check_base(
    repo: &gix::Repository,
    branch: &FullName,
) -> anyhow::Result<(String, Option<ObjectId>)> {
    let upstream =
        repo.branch_remote_tracking_ref_name(branch.as_ref(), gix::remote::Direction::Fetch);
    if let Some(Ok(upstream)) = upstream
        && let Some(mut reference) = repo.try_find_reference(upstream.as_bstr())?
    {
        return Ok((
            upstream.as_bstr().to_string(),
            Some(reference.peel_to_id()?.detach()),
        ));
    }
    Ok(("HEAD".into(), repo.head_id().ok().map(|id| id.detach())))
}

fn is_ancestor(repo: &gix::Repository, ancestor: ObjectId, of: ObjectId) -> anyhow::Result<bool> {
    Ok(ancestor == of
        || repo
            .merge_base(ancestor, of)?
            .is_some_and(|base| base == ancestor))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameBranchArgs {
    /// Branch to rename (default: the current branch).
    pub old: Option<String>,
    /// New name, e.g. `feature/login-v2`.
    pub new: String,
    /// Replace an existing branch named `new`, like `git branch -M` (default false).
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RenameBranchResult {
    /// Full new ref name, e.g. `refs/heads/feature/login-v2`.
    pub name: String,
    /// Short new name, e.g. `feature/login-v2`.
    pub short_name: String,
    /// Full SHA the branch points to (unchanged by the rename).
    pub target: String,
    /// Full ref name the branch had before, e.g. `refs/heads/feature`.
    pub renamed_from: String,
    /// True when HEAD was on the branch and now points to the new name.
    pub head_updated: bool,
    /// With `force`: the full SHA the replaced branch pointed to, so it can be recreated; `null` when
    /// no branch was replaced.
    pub replaced: Option<String>,
}

/// Rename a local branch, like `git branch -m`: its reflog and `branch.<name>.*` configuration move
/// with it, and HEAD follows when it's the current branch.
pub fn rename_branch(
    repo: &gix::Repository,
    args: RenameBranchArgs,
) -> anyhow::Result<RenameBranchResult> {
    ensure_idle(repo, "rename a branch")?;
    let old = match args.old.as_deref() {
        Some(old) => branch_ref(old)?,
        None => repo
            .head_name()?
            .context("HEAD is detached, so there is no current branch; pass `old`")?,
    };
    let new = branch_ref(&args.new)?;
    let (old_short, new_short) = (old.shorten().to_string(), new.shorten().to_string());
    if old == new {
        bail!("`{old_short}` and `{new_short}` are the same branch");
    }
    let target = direct_target(repo, &old)?.with_context(|| {
        format!("there is no branch `{old_short}`; list_refs shows the branches that exist")
    })?;
    if let Some(path) = checked_out_at(repo, old.as_ref(), true)? {
        bail!(
            "branch `{old_short}` is checked out in another worktree at {}; rename it from there",
            path.display()
        );
    }
    let replaced = direct_target(repo, &new)?;
    let expected_new = match replaced {
        None => PreviousValue::MustNotExist,
        Some(_) if !args.force => bail!(
            "branch `{new_short}` already exists; choose another name, or set `force: true` to replace it"
        ),
        Some(existing) => {
            if let Some(path) = checked_out_at(repo, new.as_ref(), false)? {
                bail!(
                    "branch `{new_short}` is checked out at {}; it can't be replaced",
                    path.display()
                );
            }
            PreviousValue::MustExistAndMatch(Target::Object(existing))
        }
    };
    committer(repo)?;
    let head_updated = repo.head_name()?.as_ref() == Some(&old);

    let message = format!("Branch: renamed {} to {}", old.as_bstr(), new.as_bstr());
    let mut edits = vec![
        RefEdit::update(new.clone(), target, expected_new, message.as_str()),
        RefEdit::delete(
            old.clone(),
            PreviousValue::MustExistAndMatch(Target::Object(target)),
        ),
    ];
    if head_updated {
        edits.push(
            RefEdit::update(
                "HEAD".try_into()?,
                Target::Symbolic(new.clone()),
                PreviousValue::MustExistAndMatch(Target::Symbolic(old.clone())),
                message.as_str(),
            )
            .with_deref(false),
        );
    }
    // Deleting the old ref deletes its reflog, so carry the history over to the new name first.
    let moved_log = copy_reflog(repo, &old, &new)?;
    if let Err(err) = repo.edit_references(edits) {
        if let Some(path) = moved_log {
            std::fs::remove_file(path).ok();
        }
        return Err(err.into());
    }
    if head_updated {
        log_head_move(repo, Some(target), target, &message)?;
    }
    rename_branch_config(repo, &old_short, &new_short).with_context(|| {
        format!(
            "the branch was renamed, but its `branch.{old_short}.*` configuration couldn't be moved \
             to `branch.{new_short}`"
        )
    })?;
    Ok(RenameBranchResult {
        name: new.as_bstr().to_string(),
        short_name: new_short,
        target: target.to_string(),
        renamed_from: old.as_bstr().to_string(),
        head_updated,
        replaced: replaced.map(|id| id.to_string()),
    })
}

/// Copy `old`'s reflog to `new`'s place, returning the new file so a failed rename can remove it.
fn copy_reflog(
    repo: &gix::Repository,
    old: &FullName,
    new: &FullName,
) -> anyhow::Result<Option<PathBuf>> {
    let logs = repo.common_dir().join("logs");
    let from = logs.join(gix::path::from_bstr(old.as_bstr())?);
    if !from.is_file() {
        return Ok(None);
    }
    let to = logs.join(gix::path::from_bstr(new.as_bstr())?);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&from, &to)
        .with_context(|| format!("cannot copy the reflog to {}", to.display()))?;
    Ok(Some(to))
}

/// Rename the `[branch "old"]` section of the repository's config to `[branch "new"]`, replacing any
/// existing `[branch "new"]`. Done under the config lock, as git does.
fn rename_branch_config(repo: &gix::Repository, old: &str, new: &str) -> anyhow::Result<()> {
    let path = repo.common_dir().join("config");
    let mut lock = gix::lock::File::acquire_to_update_resource(
        &path,
        gix::lock::acquire::Fail::Immediately,
        None,
        0,
    )?;
    let mut config = gix::config::File::from_path_no_includes(path, gix::config::Source::Local)?;
    if config.section("branch", Some(old.into())).is_err() {
        return Ok(());
    }
    while config.remove_section("branch", Some(new.into())).is_some() {}
    config.rename_section(
        "branch",
        Some(old.into()),
        "branch",
        Some(new.to_owned().into()),
    )?;
    config.write_to(&mut lock)?;
    lock.commit()?;
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTagArgs {
    /// Tag name, e.g. `v1.2.0` (`refs/tags/` is added).
    pub name: String,
    /// What to tag (default `HEAD`): any revision. Tags aren't peeled, so this can tag a tag.
    pub target: Option<String>,
    /// Message for an annotated tag, which also records you as tagger and the date. Omit it for a
    /// lightweight tag (a plain ref). Whitespace is tidied as `git tag -m` does.
    pub message: Option<String>,
    /// Replace an existing tag with the same name, like `git tag --force` (default false).
    #[serde(default)]
    pub force: bool,
}

/// Create a lightweight or annotated tag, like `git tag [-a -m <message>] <name> [<target>]`.
pub fn create_tag(repo: &gix::Repository, args: CreateTagArgs) -> anyhow::Result<ShowTagResult> {
    let name = tag_ref(&args.name)?;
    let short = name.shorten().to_string();
    let target = resolve_object(repo, args.target.as_deref())?;
    let kind = target.object()?.kind;
    let tagger = committer(repo)?;
    let expected = match direct_target(repo, &name)? {
        None => PreviousValue::MustNotExist,
        Some(existing) if args.force => PreviousValue::MustExistAndMatch(Target::Object(existing)),
        Some(existing) => bail!(
            "tag `{short}` already exists at {existing}; choose another name, or set `force: true` to replace it"
        ),
    };
    match args.message.as_deref() {
        Some(message) => {
            refuse_signing(repo, "tag.gpgSign", "tags")?;
            let message = clean_message(message)
                .context("`message` is empty; pass some text, or omit it for a lightweight tag")?;
            let mut time = gix::date::parse::TimeBuf::default();
            repo.tag(
                &short,
                target,
                kind,
                Some(tagger.to_ref(&mut time)),
                message,
                expected,
            )?;
        }
        None => {
            repo.tag_reference(&short, target, expected)?;
        }
    }
    show_tag(repo, ShowTagArgs { name: short })
}

/// The object a ref points to directly (symbolic refs are followed), or `None` when it doesn't exist.
fn direct_target(repo: &gix::Repository, name: &FullName) -> anyhow::Result<Option<ObjectId>> {
    match repo.try_find_reference(name.as_bstr())? {
        Some(mut reference) => Ok(Some(reference.follow_to_object()?.detach())),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{
        assert_unchanged, empty_repo, fixture_repo, git_stdout, open, run_git, snapshot, try_git,
        write,
    };

    fn rev(root: &Path, spec: &str) -> String {
        git_stdout(root, &["rev-parse", spec]).trim().to_owned()
    }

    fn exists(root: &Path, name: &str) -> bool {
        try_git(root, &["rev-parse", "--verify", "-q", name])
    }

    fn create(
        repo: &gix::Repository,
        name: &str,
        start: Option<&str>,
        force: bool,
    ) -> anyhow::Result<BranchResult> {
        create_branch(
            repo,
            CreateBranchArgs {
                name: name.into(),
                start_point: start.map(Into::into),
                force,
            },
        )
    }

    /// Run `f`, require it to fail with a message containing `needle`, and prove nothing on disk changed.
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

    #[test]
    fn create_branch_matches_git() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let res = create(&repo, "topic/one", Some("HEAD~1"), false).unwrap();
        assert_eq!(res.name, "refs/heads/topic/one");
        assert_eq!(res.short_name, "topic/one");
        assert_eq!(res.target, rev(root, "HEAD~1"));
        assert_eq!(rev(root, "topic/one"), res.target);
        assert!(res.previous.is_none());
        assert_eq!(rev(root, "HEAD"), rev(root, "main"), "HEAD doesn't move");
        let log = git_stdout(root, &["reflog", "show", "--format=%gs", "topic/one"]);
        assert_eq!(log.trim(), "branch: Created from HEAD~1");

        let moved = create(&repo, "topic/one", None, true).unwrap();
        assert_eq!(moved.previous.as_deref(), Some(res.target.as_str()));
        assert_eq!(rev(root, "topic/one"), rev(root, "HEAD"));
        let log = git_stdout(root, &["reflog", "show", "-1", "--format=%gs", "topic/one"]);
        assert_eq!(log.trim(), "branch: Reset to HEAD");
    }

    #[test]
    fn create_branch_refusals_change_nothing() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        refused(root, "already exists", || {
            create(&repo, "feature", None, false)
        });
        refused(root, "checked out", || {
            create(&repo, "main", Some("HEAD~1"), true)
        });
        refused(root, "valid branch name", || {
            create(&repo, "bad name", None, false)
        });
        refused(root, "cannot resolve", || {
            create(&repo, "x", Some("nope"), false)
        });

        let (dir, empty) = empty_repo();
        refused(dir.path(), "no commits yet", || {
            create(&empty, "x", None, false)
        });

        let (dir, _) = fixture_repo();
        run_git(dir.path(), &["config", "--unset", "user.name"]);
        run_git(dir.path(), &["config", "--unset", "user.email"]);
        let anonymous = open(dir.path());
        refused(dir.path(), "user.name", || {
            create(&anonymous, "x", None, false)
        });
    }

    fn delete(
        repo: &gix::Repository,
        name: &str,
        force: bool,
        dry_run: bool,
    ) -> anyhow::Result<DeleteBranchResult> {
        delete_branch(
            repo,
            DeleteBranchArgs {
                name: name.into(),
                force,
                dry_run,
            },
        )
    }

    #[test]
    fn delete_branch_is_a_dry_run_by_default() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let parsed: DeleteBranchArgs = serde_json::from_str(r#"{"name":"feature"}"#).unwrap();
        assert!(parsed.dry_run && !parsed.force);
        let before = snapshot(root);
        let res = delete_branch(&repo, parsed).unwrap();
        assert!(res.dry_run && !res.deleted && res.merged);
        assert_eq!(res.merged_into, "HEAD");
        assert_unchanged(root, &before);

        let res = delete(&repo, "feature", false, false).unwrap();
        assert!(res.deleted);
        assert!(!exists(root, "refs/heads/feature"));
        assert!(!root.join(".git/logs/refs/heads/feature").exists());
    }

    #[test]
    fn unmerged_branches_need_force() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "f.txt", "f\n");
        run_git(root, &["add", "f.txt"]);
        run_git(root, &["commit", "-q", "-m", "unmerged work"]);
        run_git(root, &["checkout", "-q", "main"]);
        run_git(root, &["config", "branch.feature.description", "keep me?"]);
        let repo = open(root);
        refused(root, "not fully merged", || {
            delete(&repo, "feature", false, true)
        });
        refused(root, "not fully merged", || {
            delete(&repo, "feature", false, false)
        });
        refused(root, "checked out", || delete(&repo, "main", true, false));
        refused(root, "no branch `nope`", || {
            delete(&repo, "nope", true, false)
        });

        let tip = rev(root, "feature");
        let res = delete(&repo, "feature", true, false).unwrap();
        assert!(!res.merged && res.deleted);
        assert_eq!(res.target, tip);
        assert!(!exists(root, "refs/heads/feature"));
        assert!(
            !try_git(root, &["config", "branch.feature.description"]),
            "config goes too"
        );
    }

    #[test]
    fn merged_into_upstream_counts() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "-b", "topic"]);
        write(root, "t.txt", "t\n");
        run_git(root, &["add", "t.txt"]);
        run_git(root, &["commit", "-q", "-m", "topic work"]);
        run_git(root, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
        run_git(
            root,
            &["config", "remote.origin.url", "https://example.com/r.git"],
        );
        run_git(
            root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        run_git(root, &["config", "branch.topic.remote", "origin"]);
        run_git(root, &["config", "branch.topic.merge", "refs/heads/topic"]);
        run_git(root, &["checkout", "-q", "main"]);
        let res = delete(&open(root), "topic", false, true).unwrap();
        assert!(res.merged);
        assert_eq!(res.merged_into, "refs/remotes/origin/topic");
    }

    fn rename(
        repo: &gix::Repository,
        old: Option<&str>,
        new: &str,
        force: bool,
    ) -> anyhow::Result<RenameBranchResult> {
        rename_branch(
            repo,
            RenameBranchArgs {
                old: old.map(Into::into),
                new: new.into(),
                force,
            },
        )
    }

    #[test]
    fn renaming_the_current_branch_moves_head_reflog_and_config() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        run_git(root, &["config", "branch.main.description", "the trunk"]);
        let old_log = git_stdout(root, &["reflog", "show", "--format=%H %gs", "main"]);
        let tip = rev(root, "main");

        let res = rename(&repo, None, "trunk", false).unwrap();
        assert!(res.head_updated);
        assert_eq!(res.renamed_from, "refs/heads/main");
        assert_eq!(res.name, "refs/heads/trunk");
        assert_eq!(res.target, tip);
        assert_eq!(
            git_stdout(root, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/trunk"
        );
        let head_log = git_stdout(root, &["reflog", "show", "-1", "--format=%gs", "HEAD"]);
        assert_eq!(
            head_log.trim(),
            "Branch: renamed refs/heads/main to refs/heads/trunk"
        );
        assert!(!exists(root, "refs/heads/main"));
        let new_log = git_stdout(root, &["reflog", "show", "--format=%H %gs", "trunk"]);
        assert!(new_log.ends_with(&old_log), "history is kept:\n{new_log}");
        assert!(new_log.starts_with(&format!(
            "{tip} Branch: renamed refs/heads/main to refs/heads/trunk"
        )));
        assert_eq!(
            git_stdout(root, &["config", "branch.trunk.description"]).trim(),
            "the trunk"
        );
        assert!(!try_git(root, &["config", "branch.main.description"]));
    }

    #[test]
    fn renaming_other_branches_and_refusals() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        refused(root, "already exists", || {
            rename(&repo, Some("feature"), "main", false)
        });
        refused(root, "no branch `nope`", || {
            rename(&repo, Some("nope"), "x", false)
        });
        refused(root, "same branch", || {
            rename(&repo, Some("feature"), "feature", false)
        });
        refused(root, "checked out", || {
            rename(&repo, Some("feature"), "main", true)
        });

        let res = rename(&repo, Some("feature"), "topic", false).unwrap();
        assert!(!res.head_updated);
        assert!(exists(root, "refs/heads/topic") && !exists(root, "refs/heads/feature"));
        assert_eq!(
            git_stdout(root, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/main"
        );

        create(&repo, "spare", Some("HEAD~1"), false).unwrap();
        let spare = rev(root, "spare");
        let replaced = rename(&repo, Some("topic"), "spare", true).unwrap();
        assert_eq!(replaced.target, rev(root, "main"));
        assert_eq!(replaced.replaced.as_deref(), Some(spare.as_str()));
        assert_eq!(rev(root, "spare"), rev(root, "main"));

        let wt = tempfile::tempdir().unwrap();
        let path = wt.path().join("linked");
        run_git(
            root,
            &["worktree", "add", "-q", path.to_str().unwrap(), "spare"],
        );
        refused(root, "another worktree", || {
            rename(&repo, Some("spare"), "elsewhere", false)
        });
    }

    #[test]
    fn rename_refuses_during_a_merge() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "ours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        let repo = open(root);
        refused(root, "in progress", || rename(&repo, None, "trunk", false));
    }

    fn tag(
        repo: &gix::Repository,
        name: &str,
        target: Option<&str>,
        message: Option<&str>,
        force: bool,
    ) -> anyhow::Result<ShowTagResult> {
        create_tag(
            repo,
            CreateTagArgs {
                name: name.into(),
                target: target.map(Into::into),
                message: message.map(Into::into),
                force,
            },
        )
    }

    #[test]
    fn lightweight_and_annotated_tags_match_git() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let light = tag(&repo, "v2", Some("HEAD~1"), None, false).unwrap();
        assert_eq!(light.kind, crate::tools::refs::TagKind::Lightweight);
        assert_eq!(rev(root, "v2"), rev(root, "HEAD~1"));

        let annotated = tag(
            &repo,
            "v3",
            None,
            Some("  Release 3\n\n\n\nnotes  \n"),
            false,
        )
        .unwrap();
        assert_eq!(annotated.kind, crate::tools::refs::TagKind::Annotated);
        assert_eq!(annotated.peeled, rev(root, "HEAD"));
        assert_eq!(rev(root, "v3^{}"), rev(root, "HEAD"));
        assert_eq!(git_stdout(root, &["cat-file", "-t", "v3"]).trim(), "tag");
        let body = git_stdout(root, &["cat-file", "tag", "v3"]);
        assert!(
            body.contains("\ntag v3\ntagger Test <test@example.com> "),
            "{body}"
        );
        assert!(body.ends_with("\n\n  Release 3\n\nnotes\n"), "{body:?}");
        assert!(
            try_git(root, &["fsck", "--strict"]),
            "objects must be valid"
        );
        assert!(
            !root.join(".git/logs/refs/tags/v3").exists(),
            "git keeps no reflog for tags"
        );

        let moved = tag(&repo, "v2", None, None, true).unwrap();
        assert_eq!(moved.target, rev(root, "HEAD"));
    }

    #[test]
    fn tag_refusals_change_nothing() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        refused(root, "already exists", || {
            tag(&repo, "v1", None, None, false)
        });
        refused(root, "message` is empty", || {
            tag(&repo, "v9", None, Some(" \n "), false)
        });
        refused(root, "valid tag name", || {
            tag(&repo, "v 9", None, None, false)
        });
        refused(root, "cannot resolve", || {
            tag(&repo, "v9", Some("nope"), None, false)
        });
        run_git(root, &["config", "tag.gpgSign", "true"]);
        let signing = open(root);
        refused(root, "can't sign tags", || {
            tag(&signing, "v9", None, Some("signed?"), false)
        });
    }
}
