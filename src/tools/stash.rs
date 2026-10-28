use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, bail};
use gix::{
    ObjectId,
    bstr::{BString, ByteSlice},
    index::entry::{Stage, Stat},
    merge::{blob::builtin_driver::text::Labels, tree::TreatAsUnresolved},
    refs::{
        Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
    status::{
        UntrackedFiles, index_worktree,
        plumbing::index_as_worktree::{Change as WorktreeChange, EntryStatus},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    checkout::{
        RestoreRequest, Step, TreeChanges, UpdatedFile, apply_restore, apply_switch, plan_restore,
        plan_switch, remove_from_worktree, staged_versions, tree_changes, write_to_worktree,
    },
    commit::write_tree,
    index_entry_kind,
    reflog::{ListStashesArgs, StashEntry, list_stashes},
    staging::{Version, filemode, replace_entries, tree_versions, worktree_blob},
    write::{committer, ensure_idle, require_worktree, update_index},
};

const STASH_REF: &str = "refs/stash";

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StashPushArgs {
    /// Description for the stash, e.g. `half-done refactor` (default: `WIP on <branch>: <commit>`).
    pub message: Option<String>,
    /// Also stash untracked files (not ignored ones), like `git stash -u` (default false).
    #[serde(default)]
    pub include_untracked: bool,
}

/// Save the working tree's and index's changes as a new stash and reset to HEAD, like `git stash push`.
pub fn stash_push(repo: &gix::Repository, args: StashPushArgs) -> anyhow::Result<StashEntry> {
    let workdir = require_worktree(repo, "stash")?.to_owned();
    ensure_idle(repo, "stash")?;
    let signature = committer(repo)?;
    let head = repo
        .head_commit()
        .ok()
        .context("HEAD has no commits yet, so there is nothing to stash against")?;
    let index = repo.index_or_empty()?;
    if index
        .entries()
        .iter()
        .any(|e| e.stage() != Stage::Unconflicted)
    {
        bail!(
            "the index has unresolved conflicts; resolve them before stashing (`conflicts` lists them)"
        );
    }
    // Build the stash in memory first: if there's nothing to save, nothing is written.
    let dry = Snapshot::take(
        &repo.clone().with_object_memory(),
        &index,
        &workdir,
        args.include_untracked,
    )?;
    let head_tree = head.tree_id()?.detach();
    if dry.index_tree == head_tree && dry.worktree_tree == head_tree && dry.untracked.is_empty() {
        bail!("there are no local changes to stash");
    }
    // Make sure the working tree can be reset afterwards before writing anything, so a refusal
    // leaves no stash behind.
    let everything = [".".to_owned()];
    let head_hex = head.id.to_string();
    let reset = RestoreRequest {
        paths: &everything,
        source: &head_hex,
        staged: true,
        worktree: true,
        overwrite_conflicts: false,
    };
    plan_restore(repo, &index, &workdir, &reset)?;
    let snapshot = Snapshot::take(repo, &index, &workdir, args.include_untracked)?;

    let branch = repo.head_name()?.map_or_else(
        || "(no branch)".to_owned(),
        |name| name.shorten().to_string(),
    );
    let about = format!(
        "{branch}: {} {}",
        head.short_id()?,
        head.message()?.summary()
    );
    let commit =
        |message: String, tree: ObjectId, parents: Vec<ObjectId>| -> anyhow::Result<ObjectId> {
            let object = gix::objs::Commit {
                tree,
                parents: parents.into_iter().collect(),
                author: signature.clone(),
                committer: signature.clone(),
                encoding: None,
                message: message.into(),
                extra_headers: Vec::new(),
            };
            Ok(repo.write_object(&object)?.detach())
        };
    let index_commit = commit(
        format!("index on {about}\n"),
        snapshot.index_tree,
        vec![head.id],
    )?;
    let mut parents = vec![head.id, index_commit];
    if let Some(tree) = snapshot.untracked_tree {
        parents.push(commit(
            format!("untracked files on {about}\n"),
            tree,
            Vec::new(),
        )?);
    }
    let message = match args.message.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => format!("On {branch}: {text}"),
        _ => format!("WIP on {about}"),
    };
    // git writes the stash commit's message without a trailing newline (unlike its parents').
    let stash = commit(message.clone(), snapshot.worktree_tree, parents)?;
    let previous = repo
        .try_find_reference(STASH_REF)?
        .map(|mut r| r.follow_to_object())
        .transpose()?;
    repo.edit_reference(RefEdit {
        change: Change::Update {
            // `refs/stash` isn't covered by `core.logAllRefUpdates`; its reflog is the stash list.
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: true,
                message: message.clone().into(),
            },
            expected: match previous {
                Some(id) => PreviousValue::MustExistAndMatch(Target::Object(id.detach())),
                None => PreviousValue::MustNotExist,
            },
            new: Target::Object(stash),
        },
        name: STASH_REF.try_into()?,
        deref: false,
    })?;

    // Like `git stash`, leave the working tree and index at HEAD, and the stashed untracked files gone.
    update_index(repo, |index| {
        let plan = plan_restore(repo, index, &workdir, &reset)?;
        apply_restore(repo, index, &workdir, &plan)
    })?;
    for path in &snapshot.untracked {
        remove_from_worktree(&workdir, path.as_bstr())?;
    }
    newest_stash(repo, 0)
}

/// The trees a stash records: the index, the working tree (tracked files), and untracked files.
struct Snapshot {
    index_tree: ObjectId,
    worktree_tree: ObjectId,
    untracked_tree: Option<ObjectId>,
    untracked: Vec<BString>,
}

impl Snapshot {
    /// Hash the index and the changed files into `repo`'s object database.
    fn take(
        repo: &gix::Repository,
        index: &gix::index::State,
        workdir: &Path,
        include_untracked: bool,
    ) -> anyhow::Result<Self> {
        let index_tree = write_tree(repo, index)?;
        let status = repo
            .status(gix::progress::Discard)?
            .untracked_files(if include_untracked {
                UntrackedFiles::Files
            } else {
                UntrackedFiles::None
            })
            .into_index_worktree_iter(Vec::<BString>::new())?;
        let (mut changed, mut untracked) = (Vec::new(), Vec::new());
        for item in status {
            match item? {
                index_worktree::Item::Modification {
                    rela_path, status, ..
                } => match status {
                    EntryStatus::Change(WorktreeChange::Removed) => changed.push((rela_path, true)),
                    EntryStatus::Change(WorktreeChange::SubmoduleModification(_))
                    | EntryStatus::NeedsUpdate(_)
                    | EntryStatus::Conflict { .. } => {}
                    EntryStatus::Change(_) | EntryStatus::IntentToAdd => {
                        changed.push((rela_path, false));
                    }
                },
                index_worktree::Item::DirectoryContents { entry, .. }
                    if entry.status == gix::dir::entry::Status::Untracked
                        && matches!(
                            entry.disk_kind,
                            Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink)
                        ) =>
                {
                    untracked.push(entry.rela_path);
                }
                _ => {}
            }
        }
        changed.sort();
        untracked.sort();

        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let filemode = filemode(repo);
        let staged = staged_versions(index);
        let mut worktree = repo.edit_tree(index_tree)?;
        for (path, removed) in &changed {
            let blob = match removed {
                true => None,
                false => {
                    let previous = staged.get(path).map(|(_, mode)| *mode);
                    worktree_blob(
                        repo,
                        &mut pipeline,
                        index,
                        workdir,
                        path.as_bstr(),
                        previous,
                        filemode,
                    )?
                }
            };
            match blob {
                Some((id, mode, _)) => {
                    worktree.upsert(path.as_bstr(), index_entry_kind(mode), id)?;
                }
                None => {
                    worktree.remove(path.as_bstr())?;
                }
            }
        }
        let worktree_tree = worktree.write()?.detach();

        let untracked_tree = if untracked.is_empty() {
            None
        } else {
            let mut tree = repo.edit_tree(ObjectId::empty_tree(repo.object_hash()))?;
            for path in &untracked {
                if let Some((id, mode, _)) = worktree_blob(
                    repo,
                    &mut pipeline,
                    index,
                    workdir,
                    path.as_bstr(),
                    None,
                    filemode,
                )? {
                    tree.upsert(path.as_bstr(), index_entry_kind(mode), id)?;
                }
            }
            Some(tree.write()?.detach())
        };
        Ok(Self {
            index_tree,
            worktree_tree,
            untracked_tree,
            untracked,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StashPopArgs {
    /// Which stash to apply: 0 (the default) is the newest, `stash@{0}`.
    #[serde(default)]
    pub index: usize,
    /// Also restore what was staged as staged, like `git stash pop --index` (default false: the
    /// changes come back unstaged, except new files, which are staged as git does).
    #[serde(default)]
    pub restore_index: bool,
    /// Drop the stash after applying it (default true). Set `false` to apply only, like `git stash apply`.
    #[serde(default = "default_true")]
    pub drop: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StashPopResult {
    /// The stash that was applied, as `list_stashes` showed it.
    pub stash: StashEntry,
    /// Tracked files the stash changed in the working tree, ordered by path; at most 500.
    pub updated: Vec<UpdatedFile>,
    /// Untracked files restored from the stash.
    pub restored_untracked: usize,
    /// True when the stash was dropped after applying it.
    pub dropped: bool,
}

/// Apply a stash to the working tree (and drop it), like `git stash pop`. Refuses instead of
/// conflicting or overwriting local changes.
pub fn stash_pop(repo: &gix::Repository, args: StashPopArgs) -> anyhow::Result<StashPopResult> {
    let workdir = require_worktree(repo, "apply a stash")?.to_owned();
    ensure_idle(repo, "apply a stash")?;
    if args.drop {
        committer(repo)?;
    }
    let stash = newest_stash(repo, args.index)?;
    let stash_commit = repo.find_commit(ObjectId::from_hex(stash.id.as_bytes())?)?;
    let parents: Vec<ObjectId> = stash_commit.parent_ids().map(|id| id.detach()).collect();
    let [base, index_commit, untracked @ ..] = parents.as_slice() else {
        bail!(
            "{} isn't a stash git can read: it has fewer than two parents",
            stash.selector
        );
    };
    let tree_of = |id: &ObjectId| -> anyhow::Result<ObjectId> {
        Ok(repo.find_commit(*id)?.tree_id()?.detach())
    };
    let trees = Trees {
        base: tree_of(base)?,
        stashed: stash_commit.tree_id()?.detach(),
        stashed_index: tree_of(index_commit)?,
        head: repo.head_tree_id()?.detach(),
        untracked: untracked.first().map(tree_of).transpose()?,
    };
    let label = format!("applying {}", stash.selector);

    // Everything is checked against an in-memory object store first, so a refusal writes nothing.
    let memory = repo.clone().with_object_memory();
    trees.plan(
        &memory,
        &*memory.index_or_empty()?,
        &workdir,
        &label,
        args.restore_index,
    )?;
    let untracked = match trees.untracked {
        Some(tree) => tree_versions(repo, tree)?,
        None => BTreeMap::new(),
    };
    for path in untracked.keys() {
        if std::fs::symlink_metadata(workdir.join(gix::path::from_bstr(path.as_bstr())?)).is_ok() {
            bail!("{label} would overwrite the untracked file `{path}`; move it away first");
        }
    }
    let mut updated = update_index(repo, |index| {
        let plan = trees.plan(repo, index, &workdir, &label, args.restore_index)?;
        let updated = trees.apply(repo, index, &workdir, plan)?;
        let files: Vec<_> = untracked.iter().map(|(p, v)| (p.clone(), *v)).collect();
        write_to_worktree(repo, &workdir, &files)?;
        Ok(updated)
    })?;
    if args.drop {
        drop_entry(repo, args.index, &stash)?;
    }
    updated.sort_by(|a, b| a.path.cmp(&b.path));
    updated.truncate(500);
    Ok(StashPopResult {
        stash,
        updated,
        restored_untracked: untracked.len(),
        dropped: args.drop,
    })
}

/// The trees involved in applying a stash.
struct Trees {
    /// HEAD when the stash was made.
    base: ObjectId,
    /// The stashed working tree.
    stashed: ObjectId,
    /// The stashed index.
    stashed_index: ObjectId,
    /// HEAD now.
    head: ObjectId,
    untracked: Option<ObjectId>,
}

/// How a stash will be applied, worked out before anything is written.
struct Plan {
    merged: ObjectId,
    index_target: Option<ObjectId>,
    changes: TreeChanges,
    steps: Vec<Step>,
}

impl Trees {
    /// Merge the stash onto HEAD in `repo` and check, with `switch`'s rule, that only clean paths
    /// would change. Refuses on conflicts or local changes in the way; writes nothing outside `repo`'s
    /// object database.
    fn plan(
        &self,
        repo: &gix::Repository,
        index: &gix::index::State,
        workdir: &Path,
        label: &str,
        restore_index: bool,
    ) -> anyhow::Result<Plan> {
        let merged = merge(repo, self.base, self.head, self.stashed, label)?;
        let index_target = restore_index
            .then(|| merge(repo, self.base, self.head, self.stashed_index, label))
            .transpose()?;
        let changes = tree_changes(repo, self.head, merged)?;
        let steps = plan_switch(repo, index, workdir, &changes, label)?;
        Ok(Plan {
            merged,
            index_target,
            changes,
            steps,
        })
    }

    /// Write the planned files and set the index to what `git stash pop` leaves.
    fn apply(
        &self,
        repo: &gix::Repository,
        index: &mut gix::index::State,
        workdir: &Path,
        plan: Plan,
    ) -> anyhow::Result<Vec<UpdatedFile>> {
        let updated = apply_switch(repo, index, workdir, plan.steps)?;
        // apply_switch staged the merged versions; most of them go back to unstaged.
        let head = tree_versions(repo, self.head)?;
        let merged = tree_versions(repo, plan.merged)?;
        let mut wanted: BTreeMap<BString, Option<Version>> = BTreeMap::new();
        match plan.index_target {
            Some(target) => {
                let staged = tree_versions(repo, target)?;
                for path in plan
                    .changes
                    .keys()
                    .chain(tree_changes(repo, self.head, target)?.keys())
                {
                    wanted.insert(path.clone(), staged.get(path).copied());
                }
            }
            None => {
                for path in plan.changes.keys() {
                    // New files stay staged, as git does; other changes come back unstaged.
                    let version = head
                        .get(path)
                        .copied()
                        .or_else(|| merged.get(path).copied());
                    wanted.insert(path.clone(), version);
                }
            }
        }
        let replacements = wanted
            .into_iter()
            .filter(|(path, version)| *version != merged.get(path).copied())
            .map(|(path, version)| (path, version.map(|(id, mode)| (id, mode, Stat::default()))))
            .collect();
        replace_entries(index, replacements);
        Ok(updated)
    }
}

/// Merge `theirs` into `ours` from `base`, refusing when it conflicts.
fn merge(
    repo: &gix::Repository,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
    label: &str,
) -> anyhow::Result<ObjectId> {
    let labels = Labels {
        ancestor: None,
        current: Some("Updated upstream".into()),
        other: Some("Stashed changes".into()),
    };
    let mut outcome = repo.merge_trees(base, ours, theirs, labels, repo.tree_merge_options()?)?;
    if outcome.has_unresolved_conflicts(TreatAsUnresolved::git()) {
        bail!(
            "{label} would conflict with HEAD; apply it with git to resolve the conflicts, or switch to \
             the commit it was made on (its `base`) first"
        );
    }
    Ok(outcome.tree.write()?.detach())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StashDropArgs {
    /// Which stash to drop: 0 (the default) is the newest, `stash@{0}`.
    #[serde(default)]
    pub index: usize,
    /// Only report which stash would be dropped (default true). Set `false` to drop it.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StashDropResult {
    /// The stash that is (or would be) dropped. Its `id` can bring it back with `git stash store`.
    pub stash: StashEntry,
    /// Whether this was a dry run; if so, nothing was changed.
    pub dry_run: bool,
}

/// Remove a stash from the stash list, like `git stash drop`. Dry run by default.
pub fn stash_drop(repo: &gix::Repository, args: StashDropArgs) -> anyhow::Result<StashDropResult> {
    let stash = newest_stash(repo, args.index)?;
    if !args.dry_run {
        committer(repo)?;
        drop_entry(repo, args.index, &stash)?;
    }
    Ok(StashDropResult {
        stash,
        dry_run: args.dry_run,
    })
}

/// `stash@{n}` as `list_stashes` describes it, or an error naming how many stashes there are.
fn newest_stash(repo: &gix::Repository, n: usize) -> anyhow::Result<StashEntry> {
    let list = list_stashes(
        repo,
        ListStashesArgs {
            limit: Some(1),
            skip: Some(n),
        },
    )?;
    list.stashes.into_iter().next().with_context(|| {
        format!("there is no stash@{{{n}}}; list_stashes shows the stashes that exist")
    })
}

/// Remove `stash@{n}` from the reflog of `refs/stash`, moving the ref when the newest one goes, as
/// git does. gix can't delete single reflog entries, so the log is rewritten under its lock.
fn drop_entry(repo: &gix::Repository, n: usize, stash: &StashEntry) -> anyhow::Result<()> {
    let log_path = repo.common_dir().join("logs").join(STASH_REF);
    let text = std::fs::read(&log_path).context("cannot read the stash list")?;
    let mut lines: Vec<&[u8]> = text.lines_with_terminator().collect();
    let position = lines
        .len()
        .checked_sub(n + 1)
        .context("the stash list changed meanwhile; list_stashes again")?;
    let new_id = lines[position].split_str(" ").nth(1).unwrap_or_default();
    if new_id != stash.id.as_bytes() {
        bail!("the stash list changed meanwhile; list_stashes again");
    }
    lines.remove(position);
    let top = ObjectId::from_hex(stash.id.as_bytes())?;
    let Some(newest) = lines.last() else {
        // The last stash: delete the ref, and its reflog with it.
        repo.edit_reference(RefEdit::delete(
            STASH_REF.try_into()?,
            PreviousValue::MustExistAndMatch(Target::Object(top)),
        ))?;
        return Ok(());
    };
    if n == 0 {
        let next = newest
            .split_str(" ")
            .nth(1)
            .context("malformed stash reflog")?;
        // Moving the ref appends a reflog line, which the rewrite below replaces.
        repo.edit_reference(RefEdit::update(
            STASH_REF.try_into()?,
            ObjectId::from_hex(next)?,
            PreviousValue::MustExistAndMatch(Target::Object(top)),
            "",
        ))?;
    }
    let mut lock = gix::lock::File::acquire_to_update_resource(
        &log_path,
        gix::lock::acquire::Fail::Immediately,
        None,
        0,
    )?;
    for line in &lines {
        std::io::Write::write_all(&mut lock, line)?;
    }
    lock.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{
        assert_unchanged, empty_repo, fixture_repo, git_stdout, ls_files, open, porcelain, run_git,
        snapshot, twins, worktree_files, write,
    };

    fn push(
        repo: &gix::Repository,
        message: Option<&str>,
        untracked: bool,
    ) -> anyhow::Result<StashEntry> {
        stash_push(
            repo,
            StashPushArgs {
                message: message.map(Into::into),
                include_untracked: untracked,
            },
        )
    }

    fn pop(
        repo: &gix::Repository,
        index: usize,
        restore_index: bool,
    ) -> anyhow::Result<StashPopResult> {
        stash_pop(
            repo,
            StashPopArgs {
                index,
                restore_index,
                drop: true,
            },
        )
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

    /// Staged and unstaged edits to the same file, a staged new file, a deletion, and an untracked file.
    fn dirty(root: &Path) {
        write(root, "b.txt", "bee\n");
        run_git(root, &["add", "b.txt"]);
        run_git(root, &["commit", "-q", "-m", "b"]);
        write(root, "a.txt", "staged\n");
        write(root, "new.txt", "brand new\n");
        run_git(root, &["add", "a.txt", "new.txt"]);
        write(root, "a.txt", "staged, then edited\n");
        std::fs::remove_file(root.join("b.txt")).unwrap();
        write(root, "loose/u.txt", "untracked\n");
    }

    fn assert_same_state(ours: &Path, theirs: &Path) {
        assert_eq!(worktree_files(ours), worktree_files(theirs));
        assert_eq!(ls_files(ours), ls_files(theirs));
        assert_eq!(porcelain(ours), porcelain(theirs));
    }

    /// The trees and message of a stash commit and its parents, which identify its content. Parent
    /// commits are compared with the repository's own HEAD, since twins' commits differ in time.
    fn shape(root: &Path, rev: &str) -> Vec<String> {
        let short_head = git_stdout(root, &["rev-parse", "--short", "HEAD"]);
        let message = git_stdout(root, &["log", "-1", "--format=%B", rev]);
        let mut out = vec![message.replace(short_head.trim(), "<head>")];
        for part in ["^{tree}", "^2^{tree}"] {
            out.push(git_stdout(root, &["rev-parse", &format!("{rev}{part}")]));
        }
        let head = git_stdout(root, &["rev-parse", "HEAD"]);
        for part in ["^2^1", "^1"] {
            let parent = git_stdout(root, &["rev-parse", &format!("{rev}{part}")]);
            out.push(format!("{part} is HEAD: {}", parent == head));
        }
        out.push(
            git_stdout(root, &["rev-list", "--parents", "-n1", rev])
                .split(' ')
                .count()
                .to_string(),
        );
        out
    }

    #[test]
    fn push_matches_git_stash() {
        for (message, untracked, flags) in [
            (None, false, &[][..]),
            (Some("half done"), false, &["-m", "half done"][..]),
            (None, true, &["-u"][..]),
        ] {
            let [(ours, repo), (theirs, _)] = twins(dirty);
            let entry = push(&repo, message, untracked).unwrap();
            let mut git = vec!["stash", "push", "-q"];
            git.extend_from_slice(flags);
            run_git(theirs.path(), &git);
            assert_same_state(ours.path(), theirs.path());
            assert_eq!(
                shape(ours.path(), "stash@{0}"),
                shape(theirs.path(), "stash@{0}"),
                "{flags:?}"
            );
            let list = |root: &Path| {
                let short_head = git_stdout(root, &["rev-parse", "--short", "HEAD"]);
                git_stdout(root, &["stash", "list", "--format=%gs"])
                    .replace(short_head.trim(), "<head>")
            };
            assert_eq!(list(ours.path()), list(theirs.path()));
            if untracked {
                assert_eq!(
                    git_stdout(ours.path(), &["rev-parse", "stash^3^{tree}"]),
                    git_stdout(theirs.path(), &["rev-parse", "stash^3^{tree}"])
                );
            }
            assert_eq!(entry.selector, "stash@{0}");
            assert_eq!(entry.includes_untracked, untracked);
        }
    }

    #[test]
    fn git_can_pop_our_stash_and_we_can_pop_gits() {
        let [(ours, repo), _] = twins(dirty);
        let original = worktree_files(ours.path());
        push(&repo, None, true).unwrap();
        run_git(ours.path(), &["stash", "pop", "-q"]);
        assert_eq!(
            worktree_files(ours.path()),
            original,
            "git restores what we stashed"
        );

        for (restore_index, flags) in [(false, &[][..]), (true, &["--index"][..])] {
            let [(ours, repo), (theirs, _)] = twins(|root| {
                dirty(root);
                run_git(root, &["stash", "push", "-q", "-u"]);
            });
            let res = pop(&repo, 0, restore_index).unwrap();
            let mut git = vec!["stash", "pop", "-q"];
            git.extend_from_slice(flags);
            run_git(theirs.path(), &git);
            assert_same_state(ours.path(), theirs.path());
            assert!(res.dropped && res.restored_untracked == 1);
            assert_eq!(git_stdout(ours.path(), &["stash", "list"]), "");
        }
    }

    #[test]
    fn pop_onto_a_moved_head_merges_like_git() {
        let moved = |root: &Path| {
            write(root, "a.txt", "one\ntwo\nthree\nfour\n");
            run_git(root, &["commit", "-q", "-am", "four lines"]);
            write(root, "a.txt", "one\ntwo\nthree\nfour, stashed\n");
            run_git(root, &["stash", "push", "-q"]);
            write(root, "a.txt", "one, committed\ntwo\nthree\nfour\n");
            write(root, "other.txt", "local edit kept\n");
            run_git(root, &["add", "other.txt"]);
            run_git(root, &["commit", "-q", "-am", "move on"]);
            write(root, "other.txt", "local edit kept, unstaged\n");
        };
        let [(ours, repo), (theirs, _)] = twins(moved);
        pop(&repo, 0, false).unwrap();
        run_git(theirs.path(), &["stash", "pop", "-q"]);
        assert_same_state(ours.path(), theirs.path());
    }

    #[test]
    fn pop_refuses_conflicts_and_obstacles() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        dirty(root);
        let repo = open(root);
        push(&repo, None, true).unwrap();
        write(root, "a.txt", "a local edit in the way\n");
        refused(root, "a.txt (unstaged changes)", || pop(&repo, 0, false));
        run_git(root, &["checkout", "--", "a.txt"]);
        write(root, "loose/u.txt", "already here\n");
        refused(root, "untracked file `loose/u.txt`", || {
            pop(&repo, 0, false)
        });
        std::fs::remove_dir_all(root.join("loose")).unwrap();
        write(root, "a.txt", "committed differently\n");
        run_git(root, &["commit", "-q", "-am", "conflicting"]);
        refused(root, "would conflict", || pop(&repo, 0, false));
        refused(root, "no stash@{3}", || pop(&repo, 3, false));
    }

    #[test]
    fn drop_matches_git_stash_drop() {
        let three = |root: &Path| {
            for n in 1..=3 {
                write(root, "a.txt", format!("edit {n}\n"));
                run_git(root, &["stash", "push", "-q", "-m", &format!("stash {n}")]);
            }
        };
        let [(ours, repo), (theirs, _)] = twins(three);
        let before = snapshot(ours.path());
        let parsed: StashDropArgs = serde_json::from_str("{}").unwrap();
        assert!(parsed.dry_run && parsed.index == 0);
        let dry = stash_drop(
            &repo,
            StashDropArgs {
                index: 1,
                dry_run: true,
            },
        )
        .unwrap();
        assert_unchanged(ours.path(), &before);
        assert_eq!(dry.stash.message, "On main: stash 2");
        // Selectors and messages; twins' stash commits differ in time, so their SHAs can differ too.
        let list = |root: &Path| git_stdout(root, &["stash", "list", "--format=%gd %gs"]);
        for (index, git) in [(1, "stash@{1}"), (0, "stash@{0}")] {
            stash_drop(
                &repo,
                StashDropArgs {
                    index,
                    dry_run: false,
                },
            )
            .unwrap();
            run_git(theirs.path(), &["stash", "drop", "-q", git]);
            assert_eq!(list(ours.path()), list(theirs.path()), "{git}");
        }
        stash_drop(
            &repo,
            StashDropArgs {
                index: 0,
                dry_run: false,
            },
        )
        .unwrap();
        assert!(!ours.path().join(".git/refs/stash").exists());
        assert_eq!(list(ours.path()), "");
    }

    #[test]
    fn submodules_are_left_alone() {
        let (sub, _) = fixture_repo();
        let url = sub.path().to_str().unwrap().to_owned();
        let with_submodule = move |root: &Path| {
            run_git(
                root,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "-q",
                    &url,
                    "sub",
                ],
            );
            run_git(root, &["commit", "-q", "-m", "add submodule"]);
            write(root, "a.txt", "edited\n");
        };
        let [(ours, repo), (theirs, _)] = twins(with_submodule);
        push(&repo, None, false).unwrap();
        run_git(theirs.path(), &["stash", "push", "-q"]);
        assert_same_state(ours.path(), theirs.path());
        pop(&repo, 0, false).unwrap();
        run_git(theirs.path(), &["stash", "pop", "-q"]);
        assert_same_state(ours.path(), theirs.path());
    }

    #[test]
    fn a_reset_that_cannot_happen_stashes_nothing() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        std::fs::remove_file(root.join("a.txt")).unwrap();
        write(
            root,
            "a.txt/inside.txt",
            "a directory where a tracked file was\n",
        );
        refused(root, "is a directory", || push(&repo, None, false));
        assert!(!root.join(".git/refs/stash").exists());
    }

    #[test]
    fn nothing_to_stash() {
        let (dir, repo) = fixture_repo();
        refused(dir.path(), "no local changes", || push(&repo, None, false));
        write(dir.path(), "untracked.txt", "only untracked\n");
        refused(dir.path(), "no local changes", || push(&repo, None, false));
        let (dir, empty) = empty_repo();
        write(dir.path(), "x.txt", "x\n");
        refused(dir.path(), "no commits yet", || push(&empty, None, true));
    }
}
