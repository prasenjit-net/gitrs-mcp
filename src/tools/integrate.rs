use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

use anyhow::{Context, bail};
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    index::entry::{Flags, Mode, Stage, Stat},
    merge::{
        blob::builtin_driver::text::Labels,
        tree::{TreatAsUnresolved, apply_index_entries::RemovalMode},
    },
    refs::{
        Target,
        transaction::{PreviousValue, RefEdit},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    LogEntry,
    checkout::{
        RestoreRequest, Step, UpdatedFile, apply_restore, apply_switch, plan_restore, plan_switch,
        tree_changes, updated_files,
    },
    commit::write_tree,
    diff::index_changes,
    in_progress, rebase, resolve_commit,
    write::{clean_message, committer, ensure_idle, require_worktree, update_index},
};

/// Files listed per call; `updated_count` always has the full number.
const MAX_LISTED: usize = 500;

/// What an operation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    /// Nothing to do: the changes are already in HEAD.
    UpToDate,
    /// The branch moved forward to the other commit without a merge commit.
    FastForward,
    /// A new commit was created (a merge, cherry-pick, revert or continued operation).
    Committed,
    /// Stopped on conflicts. Resolve the files in `conflicts`, `stage` them, then `continue_operation`
    /// (or `abort_operation`). Git can continue or abort it too.
    Stopped,
    /// The operation was aborted and the repository is back where it started.
    Aborted,
    /// A rebase finished and its branch now points to the rewritten commits.
    Finished,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RebaseProgress {
    /// Steps of the rebase plan carried out so far (picked, dropped or squashed).
    pub done: usize,
    /// Steps in the whole plan.
    pub total: usize,
    /// Full SHA of the commit being replayed when the rebase stopped; `null` otherwise.
    pub stopped_at: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OperationResult {
    /// What happened.
    pub status: OperationStatus,
    /// Full SHA HEAD points to afterwards.
    pub head: String,
    /// Short name of the branch HEAD is on afterwards; `null` when detached (as during a rebase).
    pub branch: Option<String>,
    /// The last commit created, if any.
    pub commit: Option<LogEntry>,
    /// When `stopped`: the paths with conflicts, ordered. `conflicts` shows their versions.
    pub conflicts: Vec<String>,
    /// Files changed in the working tree, ordered by path; at most 500.
    pub updated: Vec<UpdatedFile>,
    /// Number of changed files, including any beyond the listed 500.
    pub updated_count: usize,
    /// For a rebase: how far it got. `null` for other operations.
    pub rebase: Option<RebaseProgress>,
}

impl OperationResult {
    pub fn new(repo: &gix::Repository, status: OperationStatus) -> anyhow::Result<Self> {
        Ok(Self {
            status,
            head: repo.head_id()?.to_string(),
            branch: repo.head_name()?.map(|name| name.shorten().to_string()),
            commit: None,
            conflicts: Vec::new(),
            updated: Vec::new(),
            updated_count: 0,
            rebase: None,
        })
    }

    pub fn with_commit(
        mut self,
        repo: &gix::Repository,
        id: Option<ObjectId>,
    ) -> anyhow::Result<Self> {
        self.commit = id
            .map(|id| LogEntry::from_commit(&repo.find_commit(id)?))
            .transpose()?;
        Ok(self)
    }

    pub fn with_files(self, updated: Vec<UpdatedFile>, conflicts: &Merged) -> Self {
        let mut this = self.with_updated(updated);
        this.conflicts = conflicts
            .conflicts
            .keys()
            .map(ToString::to_string)
            .collect();
        this
    }

    pub fn with_updated(mut self, mut updated: Vec<UpdatedFile>) -> Self {
        updated.sort_by(|a, b| a.path.cmp(&b.path));
        self.updated_count = updated.len();
        updated.truncate(MAX_LISTED);
        self.updated = updated;
        self
    }
}

/// A three-way merge ready to apply: the tree as the working tree should look (conflicted files
/// hold conflict markers) and the index entries of conflicted paths (stages 1–3).
pub struct Merged {
    pub tree: ObjectId,
    pub conflicts: BTreeMap<BString, Vec<(Flags, ObjectId, Mode)>>,
}

impl Merged {
    fn from_outcome(
        repo: &gix::Repository,
        mut outcome: gix::merge::tree::Outcome<'_>,
    ) -> anyhow::Result<Self> {
        let tree = outcome.tree.write()?.detach();
        let mut index = repo.index_from_tree(&tree)?;
        outcome.index_changed_after_applying_conflicts(
            &mut index,
            TreatAsUnresolved::git(),
            RemovalMode::Prune,
        );
        let mut conflicts: BTreeMap<BString, Vec<_>> = BTreeMap::new();
        for entry in index
            .entries()
            .iter()
            .filter(|e| e.stage() != Stage::Unconflicted)
        {
            conflicts
                .entry(entry.path(&index).to_owned())
                .or_default()
                .push((entry.flags, entry.id, entry.mode));
        }
        Ok(Self { tree, conflicts })
    }

    /// Merge `theirs` into `ours` from `base` (trees), labelling conflict markers like git.
    pub fn trees(
        repo: &gix::Repository,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
        other_label: &str,
    ) -> anyhow::Result<Self> {
        let labels = Labels {
            ancestor: None,
            current: Some("HEAD".into()),
            other: Some(other_label.as_bytes().as_bstr()),
        };
        let outcome = repo.merge_trees(base, ours, theirs, labels, repo.tree_merge_options()?)?;
        Self::from_outcome(repo, outcome)
    }

    fn commits(
        repo: &gix::Repository,
        ours: ObjectId,
        theirs: ObjectId,
        other_label: &str,
    ) -> anyhow::Result<Self> {
        let labels = Labels {
            ancestor: None,
            current: Some("HEAD".into()),
            other: Some(other_label.as_bytes().as_bstr()),
        };
        let options = gix::merge::commit::Options::from(repo.tree_merge_options()?)
            .with_allow_missing_merge_base(true);
        let outcome = repo.merge_commits(ours, theirs, labels, options)?;
        Self::from_outcome(repo, outcome.tree_merge)
    }

    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Check that applying `merged` on top of `head_tree` only rewrites paths without local changes.
pub fn plan_merged(
    repo: &gix::Repository,
    index: &gix::index::State,
    workdir: &Path,
    head_tree: ObjectId,
    merged: &Merged,
    action: &str,
) -> anyhow::Result<Vec<Step>> {
    let changes = tree_changes(repo, head_tree, merged.tree)?;
    plan_switch(repo, index, workdir, &changes, action)
}

/// Write the merged files and put conflicted paths into the index as stages 1–3.
pub fn apply_merged(
    repo: &gix::Repository,
    index: &mut gix::index::State,
    workdir: &Path,
    steps: Vec<Step>,
    merged: &Merged,
) -> anyhow::Result<Vec<UpdatedFile>> {
    let updated = apply_switch(repo, index, workdir, steps)?;
    let conflicted: HashSet<&BStr> = merged.conflicts.keys().map(|p| p.as_bstr()).collect();
    index.remove_entries(|_, path, _| conflicted.contains(path));
    for (path, entries) in &merged.conflicts {
        for (flags, id, mode) in entries {
            index.dangerously_push_entry(Stat::default(), *id, *flags, *mode, path.as_bstr());
        }
    }
    Ok(updated)
}

/// `message` followed by git's list of conflicted paths, as written to `MERGE_MSG`.
pub fn with_conflicts(message: &str, merged: &Merged) -> String {
    let mut text = message.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if !merged.is_clean() {
        text.push_str("\n# Conflicts:\n");
        for path in merged.conflicts.keys() {
            text.push_str(&format!("#\t{path}\n"));
        }
    }
    text
}

/// Write a commit object (author defaults to the committer) without moving any ref.
pub fn new_commit(
    repo: &gix::Repository,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Option<gix::actor::Signature>,
    message: &str,
) -> anyhow::Result<ObjectId> {
    let committer = committer(repo)?;
    let commit = gix::objs::Commit {
        tree,
        parents: parents.into_iter().collect(),
        author: author.unwrap_or_else(|| committer.clone()),
        committer,
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    Ok(repo.write_object(&commit)?.detach())
}

/// Move HEAD (and the branch it's on) from `from` to `to`, compare-and-swap, with `message` in the reflog.
pub fn move_head(
    repo: &gix::Repository,
    from: ObjectId,
    to: ObjectId,
    message: &str,
) -> anyhow::Result<()> {
    repo.edit_reference(
        RefEdit::update(
            "HEAD".try_into()?,
            to,
            PreviousValue::MustExistAndMatch(Target::Object(from)),
            message,
        )
        .with_deref(true),
    )?;
    Ok(())
}

/// Write one of git's operation state files (`MERGE_HEAD`, `MERGE_MSG`, ...) in the git directory.
pub fn write_state(repo: &gix::Repository, name: &str, content: &str) -> anyhow::Result<()> {
    let path = repo.git_dir().join(name);
    std::fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))
}

pub fn read_state(repo: &gix::Repository, name: &str) -> anyhow::Result<String> {
    let path = repo.git_dir().join(name);
    std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))
}

pub fn remove_state(repo: &gix::Repository, names: &[&str]) {
    for name in names {
        std::fs::remove_file(repo.git_dir().join(name)).ok();
    }
}

/// The commit a state file such as `MERGE_HEAD` names.
fn state_commit(repo: &gix::Repository, name: &str) -> anyhow::Result<ObjectId> {
    let text = read_state(repo, name)?;
    let first = text.lines().next().context("empty state file")?;
    Ok(ObjectId::from_hex(first.trim().as_bytes())?)
}

/// Strip `#` comment lines, then tidy whitespace, as `git commit` does for a message it would edit.
fn strip_comments(message: &str) -> Option<String> {
    let kept: Vec<&str> = message
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect();
    clean_message(&kept.join("\n"))
}

/// The commit's summary for reflog messages.
fn summary(message: &str) -> &str {
    message.lines().next().unwrap_or_default()
}

/// Refuse when the index differs from HEAD: like git's merge machinery, these operations need it clean.
fn ensure_index_matches_head(
    repo: &gix::Repository,
    head_tree: ObjectId,
    action: &str,
) -> anyhow::Result<()> {
    let (staged, unmerged) = index_changes(repo, &head_tree, &[])?;
    if !staged.is_empty() || !unmerged.is_empty() {
        bail!("cannot {action}: the index has staged changes; commit, unstage or stash them first");
    }
    Ok(())
}

/// What a three-way operation should do once its merge is computed.
struct Plan<'a> {
    action: &'a str,
    head: ObjectId,
    head_tree: ObjectId,
    /// Produces the merge in the given repository (in memory for checks, then for real).
    merge: &'a dyn Fn(&gix::Repository) -> anyhow::Result<Merged>,
}

/// Compute the merge in memory and check it can be applied, then apply it for real under the index
/// lock. Returns the real merge and the files written.
fn run(
    repo: &gix::Repository,
    workdir: &Path,
    plan: &Plan<'_>,
) -> anyhow::Result<(Merged, Vec<UpdatedFile>)> {
    let memory = repo.clone().with_object_memory();
    let merged = (plan.merge)(&memory)?;
    plan_merged(
        &memory,
        &*memory.index_or_empty()?,
        workdir,
        plan.head_tree,
        &merged,
        plan.action,
    )?;
    let merged = (plan.merge)(repo)?;
    let updated = update_index(repo, |index| {
        let steps = plan_merged(repo, index, workdir, plan.head_tree, &merged, plan.action)?;
        apply_merged(repo, index, workdir, steps, &merged)
    })?;
    Ok((merged, updated))
}

/// How `merge` may fast-forward.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FastForward {
    /// Fast-forward when possible, otherwise create a merge commit (the default, like `git merge`).
    #[default]
    Allow,
    /// Always create a merge commit, like `git merge --no-ff`.
    Never,
    /// Only fast-forward; refuse when a merge commit would be needed, like `git merge --ff-only`.
    Only,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergeArgs {
    /// Branch, tag or commit to merge into the current branch, e.g. `feature`.
    pub theirs: String,
    /// Message for the merge commit (default: git's, e.g. `Merge branch 'feature'`).
    pub message: Option<String>,
    /// `allow` (default), `never` (`--no-ff`) or `only` (`--ff-only`).
    #[serde(default)]
    pub fast_forward: FastForward,
}

/// Merge a branch or commit into HEAD, like `git merge`.
pub fn merge(repo: &gix::Repository, args: MergeArgs) -> anyhow::Result<OperationResult> {
    let workdir = require_worktree(repo, "merge")?.to_owned();
    ensure_idle(repo, "merge")?;
    committer(repo)?;
    let head_commit = repo
        .head_commit()
        .ok()
        .context("HEAD has no commits yet; commit first")?;
    let head = head_commit.id;
    let head_tree = head_commit.tree_id()?.detach();
    let theirs = resolve_commit(repo, Some(&args.theirs))?;
    let theirs_id = theirs.id;
    let base = repo.merge_base(head, theirs_id)?.map(|id| id.detach());
    if base == Some(theirs_id) {
        return OperationResult::new(repo, OperationStatus::UpToDate);
    }
    if base == Some(head) && args.fast_forward != FastForward::Never {
        let changes = tree_changes(repo, head_tree, theirs.tree_id()?.detach())?;
        let action = format!("merging `{}`", args.theirs);
        let updated = update_index(repo, |index| {
            let steps = plan_switch(repo, index, &workdir, &changes, &action)?;
            let updated = apply_switch(repo, index, &workdir, steps)?;
            move_head(
                repo,
                head,
                theirs_id,
                &format!("merge {}: Fast-forward", args.theirs),
            )?;
            Ok(updated)
        })?;
        write_state(repo, "ORIG_HEAD", &format!("{head}\n"))?;
        let empty = Merged {
            tree: head_tree,
            conflicts: BTreeMap::new(),
        };
        return Ok(
            OperationResult::new(repo, OperationStatus::FastForward)?.with_files(updated, &empty)
        );
    }
    if args.fast_forward == FastForward::Only {
        bail!(
            "`{}` can't be fast-forwarded to; merge with `fast_forward: allow` instead",
            args.theirs
        );
    }
    ensure_index_matches_head(repo, head_tree, "merge")?;

    let message = match args.message.as_deref() {
        Some(text) => clean_message(text).context("`message` is empty")?,
        None => format!("{}\n", default_merge_message(repo, &args.theirs)?),
    };
    let action = format!("merging `{}`", args.theirs);
    let plan = Plan {
        action: &action,
        head,
        head_tree,
        merge: &|r| Merged::commits(r, head, theirs_id, &args.theirs),
    };
    let (merged, updated) = run(repo, &workdir, &plan)?;
    write_state(repo, "ORIG_HEAD", &format!("{}\n", plan.head))?;
    if !merged.is_clean() {
        write_state(repo, "MERGE_HEAD", &format!("{theirs_id}\n"))?;
        write_state(repo, "MERGE_MSG", &with_conflicts(&message, &merged))?;
        let mode = if args.fast_forward == FastForward::Never {
            "no-ff"
        } else {
            ""
        };
        write_state(repo, "MERGE_MODE", mode)?;
        return Ok(
            OperationResult::new(repo, OperationStatus::Stopped)?.with_files(updated, &merged)
        );
    }
    let commit = new_commit(repo, merged.tree, vec![head, theirs_id], None, &message)?;
    move_head(
        repo,
        head,
        commit,
        &format!("merge {}: Merge made by the 'ort' strategy.", args.theirs),
    )?;
    OperationResult::new(repo, OperationStatus::Committed)?
        .with_commit(repo, Some(commit))
        .map(|r| r.with_files(updated, &merged))
}

/// git's default merge message: `Merge branch 'x'`, `Merge tag 'v1'`, `Merge remote-tracking branch
/// 'origin/x'` or `Merge commit 'sha'`, with ` into <branch>` unless merging into main or master.
fn default_merge_message(repo: &gix::Repository, theirs: &str) -> anyhow::Result<String> {
    let kind = [
        ("refs/heads/", "branch"),
        ("refs/tags/", "tag"),
        ("refs/remotes/", "remote-tracking branch"),
    ]
    .into_iter()
    .find(|(prefix, _)| {
        repo.try_find_reference(format!("{prefix}{theirs}").as_str())
            .ok()
            .flatten()
            .is_some()
    })
    .map_or("commit", |(_, kind)| kind);
    let mut message = format!("Merge {kind} '{theirs}'");
    match repo.head_name()? {
        Some(name) if matches!(name.shorten().to_str(), Ok("main" | "master")) => {}
        Some(name) => message.push_str(&format!(" into {}", name.shorten())),
        None => message.push_str(" into HEAD"),
    }
    Ok(message)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PickArgs {
    /// The commit to apply, e.g. `feature~2` or a SHA. Merge commits aren't supported.
    pub rev: String,
}

/// Apply the change one commit introduced onto HEAD as a new commit, like `git cherry-pick`.
pub fn cherry_pick(repo: &gix::Repository, args: PickArgs) -> anyhow::Result<OperationResult> {
    replay(repo, &args.rev, Direction::Forward)
}

/// Commit the inverse of one commit's change onto HEAD, like `git revert`.
pub fn revert(repo: &gix::Repository, args: PickArgs) -> anyhow::Result<OperationResult> {
    replay(repo, &args.rev, Direction::Backward)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

fn replay(
    repo: &gix::Repository,
    rev: &str,
    direction: Direction,
) -> anyhow::Result<OperationResult> {
    let verb = match direction {
        Direction::Forward => "cherry-pick",
        Direction::Backward => "revert",
    };
    let workdir = require_worktree(repo, verb)?.to_owned();
    ensure_idle(repo, verb)?;
    committer(repo)?;
    let head_commit = repo
        .head_commit()
        .ok()
        .context("HEAD has no commits yet; commit first")?;
    let head = head_commit.id;
    let head_tree = head_commit.tree_id()?.detach();
    ensure_index_matches_head(repo, head_tree, verb)?;
    let picked = resolve_commit(repo, Some(rev))?;
    let parents: Vec<ObjectId> = picked.parent_ids().map(|id| id.detach()).collect();
    if parents.len() > 1 {
        bail!("`{rev}` is a merge commit; {verb}ing merges isn't supported, use git with `-m`");
    }
    let parent_tree = match parents.first() {
        Some(parent) => repo.find_commit(*parent)?.tree_id()?.detach(),
        None => ObjectId::empty_tree(repo.object_hash()),
    };
    let picked_tree = picked.tree_id()?.detach();
    let original = picked.message_raw()?.to_string();
    let label = format!("{} ({})", picked.short_id()?, picked.message()?.summary());
    let (base, theirs, label, message, author, state) = match direction {
        Direction::Forward => (
            parent_tree,
            picked_tree,
            label,
            original,
            Some(picked.author()?.to_owned()?),
            "CHERRY_PICK_HEAD",
        ),
        Direction::Backward => (
            picked_tree,
            parent_tree,
            format!("parent of {label}"),
            revert_message(
                picked.message()?.summary().to_str_lossy().as_ref(),
                picked.id,
            ),
            None,
            "REVERT_HEAD",
        ),
    };
    let action = format!("{verb}ing `{rev}`");
    let plan = Plan {
        action: &action,
        head,
        head_tree,
        merge: &|r| Merged::trees(r, base, head_tree, theirs, &label),
    };
    let memory = repo.clone().with_object_memory();
    let preview = (plan.merge)(&memory)?;
    if preview.is_clean() && preview.tree == head_tree {
        bail!("{action} would change nothing: its changes are already in HEAD");
    }
    let (merged, updated) = run(repo, &workdir, &plan)?;
    if !merged.is_clean() {
        write_state(repo, state, &format!("{}\n", picked.id))?;
        write_state(repo, "MERGE_MSG", &with_conflicts(&message, &merged))?;
        return Ok(
            OperationResult::new(repo, OperationStatus::Stopped)?.with_files(updated, &merged)
        );
    }
    let commit = new_commit(repo, merged.tree, vec![head], author, &message)?;
    move_head(
        repo,
        head,
        commit,
        &format!("{verb}: {}", summary(&message)),
    )?;
    OperationResult::new(repo, OperationStatus::Committed)?
        .with_commit(repo, Some(commit))
        .map(|r| r.with_files(updated, &merged))
}

/// git's revert message; reverting a revert says `Reapply` instead.
fn revert_message(subject: &str, id: ObjectId) -> String {
    let title = match subject
        .strip_prefix("Revert \"")
        .and_then(|s| s.strip_suffix('"'))
    {
        Some(inner) => format!("Reapply \"{inner}\""),
        None => format!("Revert \"{subject}\""),
    };
    format!("{title}\n\nThis reverts commit {id}.\n")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContinueArgs {
    /// Message for the commit that concludes a merge, cherry-pick or revert (default: the one git
    /// prepared in `MERGE_MSG`, without its `#` comment lines). Ignored for a rebase.
    pub message: Option<String>,
    /// For a rebase: leave out the commit that stopped instead of committing it, like
    /// `git rebase --skip` (default false).
    #[serde(default)]
    pub skip: bool,
}

/// Conclude the merge, cherry-pick, revert or rebase that stopped on conflicts, once they're
/// resolved and staged, like `git commit` / `git rebase --continue`.
pub fn continue_operation(
    repo: &gix::Repository,
    args: ContinueArgs,
) -> anyhow::Result<OperationResult> {
    require_worktree(repo, "continue")?;
    let state = repo
        .state()
        .context("nothing to continue: no merge, cherry-pick, revert or rebase is in progress")?;
    use gix::state::InProgress;
    if matches!(state, InProgress::Rebase | InProgress::RebaseInteractive) {
        let how = if args.skip {
            rebase::Resume::Skip
        } else {
            rebase::Resume::Continue
        };
        return rebase::resume(repo, how);
    }
    if args.skip {
        bail!("`skip` only applies to a rebase");
    }
    let (state_file, reflog_kind) = match state {
        InProgress::Merge => ("MERGE_HEAD", "commit (merge)"),
        InProgress::CherryPick => ("CHERRY_PICK_HEAD", "commit (cherry-pick)"),
        InProgress::Revert => ("REVERT_HEAD", "commit"),
        other => bail!("continuing a {other:?} isn't supported; use git"),
    };
    let index = repo.index_or_empty()?;
    if index
        .entries()
        .iter()
        .any(|e| e.stage() != Stage::Unconflicted)
    {
        bail!(
            "there are still unresolved conflicts; resolve and `stage` them first (`conflicts` lists them)"
        );
    }
    let head = repo.head_id()?.detach();
    let other = state_commit(repo, state_file)?;
    let message = match args.message.as_deref() {
        Some(text) => clean_message(text).context("`message` is empty")?,
        None => strip_comments(&read_state(repo, "MERGE_MSG").unwrap_or_default())
            .context("the prepared message is empty; pass `message`")?,
    };
    let tree = write_tree(repo, &index)?;
    let (parents, author) = match state {
        InProgress::Merge => (vec![head, other], None),
        InProgress::CherryPick => (
            vec![head],
            Some(repo.find_commit(other)?.author()?.to_owned()?),
        ),
        _ => (vec![head], None),
    };
    let commit = new_commit(repo, tree, parents, author, &message)?;
    move_head(
        repo,
        head,
        commit,
        &format!("{reflog_kind}: {}", summary(&message)),
    )?;
    remove_state(repo, &[state_file, "MERGE_MSG", "MERGE_MODE", "AUTO_MERGE"]);
    OperationResult::new(repo, OperationStatus::Committed)?.with_commit(repo, Some(commit))
}

/// Abandon the merge, cherry-pick, revert or rebase in progress and go back to where it started,
/// like `git merge --abort` / `git rebase --abort`. Local changes to files it didn't touch are kept.
pub fn abort_operation(repo: &gix::Repository) -> anyhow::Result<OperationResult> {
    let workdir = require_worktree(repo, "abort")?.to_owned();
    let state = in_progress(repo).context("nothing to abort: no operation is in progress")?;
    use gix::state::InProgress;
    if matches!(
        repo.state(),
        Some(InProgress::Rebase | InProgress::RebaseInteractive)
    ) {
        return rebase::resume(repo, rebase::Resume::Abort);
    }
    if !matches!(
        repo.state(),
        Some(InProgress::Merge | InProgress::CherryPick | InProgress::Revert)
    ) {
        bail!("aborting a {state} isn't supported; use git");
    }
    let head_tree = repo.head_tree_id()?.detach();
    let updated = restore_touched(repo, &workdir, head_tree)?;
    remove_state(
        repo,
        &[
            "MERGE_HEAD",
            "MERGE_MSG",
            "MERGE_MODE",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "AUTO_MERGE",
        ],
    );
    Ok(OperationResult::new(repo, OperationStatus::Aborted)?.with_updated(updated))
}

/// Restore every path whose index entry differs from `tree` (staged or conflicted) in both the index
/// and the working tree, leaving unstaged changes to other paths alone. Returns the files rewritten.
pub fn restore_touched(
    repo: &gix::Repository,
    workdir: &Path,
    tree: ObjectId,
) -> anyhow::Result<Vec<UpdatedFile>> {
    let (staged, unmerged) = index_changes(repo, &tree, &[])?;
    let paths: Vec<String> = staged
        .iter()
        .flat_map(|c| {
            [
                Some(c.path().to_string()),
                c.old_path().map(|p| p.to_string()),
            ]
        })
        .flatten()
        .chain(unmerged)
        .map(|path| format!(":(literal){path}"))
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let source = tree.to_string();
    let request = RestoreRequest {
        paths: &paths,
        source: &source,
        staged: true,
        worktree: true,
        overwrite_conflicts: true,
    };
    update_index(repo, |index| {
        let plan = plan_restore(repo, index, workdir, &request)?;
        apply_restore(repo, index, workdir, &plan)?;
        Ok(updated_files(&plan))
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::{
        ChangeKind,
        testutil::{
            assert_unchanged, diverged, git_stdout, ls_files, open, pinned_git as git,
            pinned_twins, porcelain, snapshot, try_git, worktree_files, write,
        },
    };

    fn rev(root: &Path, spec: &str) -> String {
        git_stdout(root, &["rev-parse", spec]).trim().to_owned()
    }

    fn last_reflog(root: &Path) -> String {
        git_stdout(root, &["reflog", "show", "-1", "--format=%gs", "HEAD"])
            .trim()
            .to_owned()
    }

    fn assert_same(ours: &Path, theirs: &Path) {
        assert_eq!(worktree_files(ours), worktree_files(theirs));
        assert_eq!(ls_files(ours), ls_files(theirs));
        assert_eq!(porcelain(ours), porcelain(theirs));
    }

    fn merge_args(theirs: &str, fast_forward: FastForward) -> MergeArgs {
        MergeArgs {
            theirs: theirs.into(),
            message: None,
            fast_forward,
        }
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

    #[test]
    fn clean_merge_matches_git() {
        let [(ours, repo), (theirs, _)] = pinned_twins(|r| diverged(r, false));
        let res = merge(&repo, merge_args("feature", FastForward::Allow)).unwrap();
        git(theirs.path(), &["merge", "-q", "--no-edit", "feature"]);
        assert_eq!(res.status, OperationStatus::Committed);
        assert_same(ours.path(), theirs.path());
        for spec in ["HEAD^{tree}", "HEAD^1", "HEAD^2"] {
            assert_eq!(rev(ours.path(), spec), rev(theirs.path(), spec), "{spec}");
        }
        let body = |r: &Path| git_stdout(r, &["log", "-1", "--format=%B"]);
        assert_eq!(body(ours.path()), body(theirs.path()));
        assert_eq!(last_reflog(ours.path()), last_reflog(theirs.path()));
        assert_eq!(rev(ours.path(), "ORIG_HEAD"), rev(ours.path(), "HEAD^1"));
    }

    #[test]
    fn fast_forward_up_to_date_and_modes() {
        let [(ours, repo), (theirs, _)] = pinned_twins(|r| {
            diverged(r, false);
            git(r, &["checkout", "-q", "-b", "behind", "main~1"]);
        });
        let res = merge(&repo, merge_args("main", FastForward::Allow)).unwrap();
        git(theirs.path(), &["merge", "-q", "main"]);
        assert_eq!(res.status, OperationStatus::FastForward);
        assert_same(ours.path(), theirs.path());
        assert_eq!(rev(ours.path(), "HEAD"), rev(theirs.path(), "HEAD"));
        assert_eq!(last_reflog(ours.path()), last_reflog(theirs.path()));

        let res = merge(&repo, merge_args("main~1", FastForward::Allow)).unwrap();
        assert_eq!(res.status, OperationStatus::UpToDate);
        refused(ours.path(), "can't be fast-forwarded", || {
            merge(&repo, merge_args("feature", FastForward::Only))
        });

        let [(ours, repo), (theirs, _)] = pinned_twins(|r| {
            diverged(r, false);
            git(r, &["checkout", "-q", "-b", "topic", "main~1"]);
        });
        merge(&repo, merge_args("main", FastForward::Never)).unwrap();
        git(
            theirs.path(),
            &["merge", "-q", "--no-ff", "--no-edit", "main"],
        );
        assert_eq!(
            rev(ours.path(), "HEAD^{tree}"),
            rev(theirs.path(), "HEAD^{tree}")
        );
        let body = |r: &Path| git_stdout(r, &["log", "-1", "--format=%B"]);
        assert_eq!(
            body(ours.path()),
            body(theirs.path()),
            "`into topic` on other branches"
        );
    }

    #[test]
    fn conflicted_merge_matches_git_and_git_can_finish_it() {
        let [(ours, repo), (theirs, _)] = pinned_twins(|r| diverged(r, true));
        let res = merge(&repo, merge_args("feature", FastForward::Allow)).unwrap();
        assert!(!try_git(theirs.path(), &["merge", "-q", "feature"]));
        assert_eq!(res.status, OperationStatus::Stopped);
        assert_eq!(res.conflicts, ["a"]);
        assert_same(ours.path(), theirs.path());
        for state in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
            let read = |r: &Path| std::fs::read_to_string(r.join(".git").join(state)).unwrap();
            assert_eq!(read(ours.path()), read(theirs.path()), "{state}");
        }
        // git finishes what gitrs started, and the reverse.
        for root in [ours.path(), theirs.path()] {
            write(root, "a", "resolved\n");
            git(root, &["add", "a"]);
        }
        git(ours.path(), &["commit", "-q"]);
        let finished = continue_operation(
            &open(theirs.path()),
            ContinueArgs {
                message: None,
                skip: false,
            },
        )
        .unwrap();
        assert_eq!(finished.status, OperationStatus::Committed);
        for spec in ["HEAD^{tree}", "HEAD^1", "HEAD^2"] {
            assert_eq!(rev(ours.path(), spec), rev(theirs.path(), spec), "{spec}");
        }
        let body = |r: &Path| git_stdout(r, &["log", "-1", "--format=%B"]);
        assert_eq!(body(ours.path()), body(theirs.path()));
        assert_eq!(last_reflog(ours.path()), last_reflog(theirs.path()));
        assert!(!theirs.path().join(".git/MERGE_HEAD").exists());
    }

    pub(crate) fn updated(res: &OperationResult) -> Vec<(String, ChangeKind)> {
        res.updated
            .iter()
            .map(|f| (f.path.clone(), f.change))
            .collect()
    }

    #[test]
    fn abort_matches_git_merge_abort() {
        let [(ours, _), (theirs, _)] = pinned_twins(|r| {
            diverged(r, true);
            write(r, "b", "a local edit merge doesn't touch\n");
        });
        for root in [ours.path(), theirs.path()] {
            assert!(!try_git(root, &["merge", "-q", "feature"]));
        }
        let res = abort_operation(&open(ours.path())).unwrap();
        git(theirs.path(), &["merge", "--abort"]);
        assert_eq!(res.status, OperationStatus::Aborted);
        // The conflicted file loses its markers and the file the merge brought in goes.
        assert_eq!(
            updated(&res),
            [
                ("a".to_owned(), ChangeKind::Modified),
                ("c".to_owned(), ChangeKind::Deleted)
            ]
        );
        assert_same(ours.path(), theirs.path());
        assert!(!ours.path().join(".git/MERGE_HEAD").exists());
        assert_eq!(
            std::fs::read_to_string(ours.path().join("b")).unwrap(),
            "a local edit merge doesn't touch\n"
        );

        // And git can abort a merge gitrs started.
        let [(dir, repo), _] = pinned_twins(|r| diverged(r, true));
        merge(&repo, merge_args("feature", FastForward::Allow)).unwrap();
        git(dir.path(), &["merge", "--abort"]);
        assert_eq!(porcelain(dir.path()), "");
    }

    #[test]
    fn merge_refusals_change_nothing() {
        let [(dir, repo), _] = pinned_twins(|r| diverged(r, true));
        let root = dir.path();
        write(root, "b", "staged\n");
        git(root, &["add", "b"]);
        refused(root, "staged changes", || {
            merge(&repo, merge_args("feature", FastForward::Allow))
        });
        git(root, &["reset", "-q"]);
        write(root, "b", "bee\n");
        write(root, "c", "untracked where feature adds c\n");
        refused(root, "c (untracked file in the way)", || {
            merge(&repo, merge_args("feature", FastForward::Allow))
        });
        std::fs::remove_file(root.join("c")).unwrap();
        refused(root, "nothing to continue", || {
            continue_operation(
                &repo,
                ContinueArgs {
                    message: None,
                    skip: false,
                },
            )
        });
        refused(root, "nothing to abort", || abort_operation(&repo));
        refused(root, "cannot resolve", || {
            merge(&repo, merge_args("nope", FastForward::Allow))
        });

        merge(&repo, merge_args("feature", FastForward::Allow)).unwrap();
        refused(root, "in progress", || {
            merge(&repo, merge_args("feature", FastForward::Allow))
        });
        refused(root, "unresolved conflicts", || {
            continue_operation(
                &repo,
                ContinueArgs {
                    message: None,
                    skip: false,
                },
            )
        });
        refused(root, "only applies to a rebase", || {
            continue_operation(
                &repo,
                ContinueArgs {
                    message: None,
                    skip: true,
                },
            )
        });
    }

    #[test]
    fn cherry_pick_and_revert_match_git() {
        let [(ours, repo), (theirs, _)] = pinned_twins(|r| diverged(r, false));
        let res = cherry_pick(
            &repo,
            PickArgs {
                rev: "feature".into(),
            },
        )
        .unwrap();
        git(theirs.path(), &["cherry-pick", "feature"]);
        assert_eq!(res.status, OperationStatus::Committed);
        assert_same(ours.path(), theirs.path());
        let shape = |r: &Path| git_stdout(r, &["log", "-1", "--format=%T %an %ae %ad|%B"]);
        assert_eq!(shape(ours.path()), shape(theirs.path()));
        assert_eq!(last_reflog(ours.path()), last_reflog(theirs.path()));

        revert(
            &repo,
            PickArgs {
                rev: "HEAD~1".into(),
            },
        )
        .unwrap();
        git(theirs.path(), &["revert", "--no-edit", "HEAD~1"]);
        assert_same(ours.path(), theirs.path());
        let shape = |r: &Path| git_stdout(r, &["log", "-1", "--format=%T|%B"]);
        assert_eq!(shape(ours.path()), shape(theirs.path()));
        assert_eq!(last_reflog(ours.path()), last_reflog(theirs.path()));
        assert_eq!(
            revert_message("Revert \"x\"", repo.head_id().unwrap().detach())
                .lines()
                .next(),
            Some("Reapply \"x\"")
        );
    }

    #[test]
    fn conflicted_picks_match_git_and_continue() {
        for (verb, args) in [
            ("cherry-pick", &["cherry-pick", "feature~1"][..]),
            ("revert", &["revert", "--no-edit", "HEAD"][..]),
        ] {
            let setup = |r: &Path| {
                diverged(r, true);
                if verb == "revert" {
                    write(r, "a", "ours, then more\n");
                    git(r, &["commit", "-q", "-am", "more"]);
                    git(r, &["revert", "--no-edit", "HEAD"]);
                    write(r, "a", "different again\n");
                    git(r, &["commit", "-q", "-am", "again"]);
                }
            };
            let [(ours, repo), (theirs, _)] = pinned_twins(setup);
            let target = if verb == "revert" {
                "HEAD~1"
            } else {
                "feature~1"
            };
            let res = if verb == "revert" {
                revert(&repo, PickArgs { rev: target.into() }).unwrap()
            } else {
                cherry_pick(&repo, PickArgs { rev: target.into() }).unwrap()
            };
            let mut git_args = args.to_vec();
            *git_args.last_mut().unwrap() = target;
            assert!(!try_git(theirs.path(), &git_args), "{verb}");
            assert_eq!(res.status, OperationStatus::Stopped, "{verb}");
            assert_same(ours.path(), theirs.path());
            let state = if verb == "revert" {
                "REVERT_HEAD"
            } else {
                "CHERRY_PICK_HEAD"
            };
            for file in [state, "MERGE_MSG"] {
                let read = |r: &Path| std::fs::read_to_string(r.join(".git").join(file)).unwrap();
                assert_eq!(read(ours.path()), read(theirs.path()), "{verb} {file}");
            }
            for root in [ours.path(), theirs.path()] {
                write(root, "a", "resolved\n");
                git(root, &["add", "a"]);
            }
            continue_operation(
                &repo,
                ContinueArgs {
                    message: None,
                    skip: false,
                },
            )
            .unwrap();
            git(theirs.path(), &[verb, "--continue"]);
            let shape = |r: &Path| git_stdout(r, &["log", "-1", "--format=%T %an|%B"]);
            assert_eq!(shape(ours.path()), shape(theirs.path()), "{verb}");
            assert_eq!(
                last_reflog(ours.path()),
                last_reflog(theirs.path()),
                "{verb}"
            );
        }
    }

    #[test]
    fn pick_refusals() {
        let [(dir, repo), _] = pinned_twins(|r| {
            diverged(r, false);
            git(r, &["merge", "-q", "--no-edit", "feature"]);
        });
        let root = dir.path();
        refused(root, "merge commit", || {
            cherry_pick(&repo, PickArgs { rev: "HEAD".into() })
        });
        refused(root, "already in HEAD", || {
            cherry_pick(
                &repo,
                PickArgs {
                    rev: "feature".into(),
                },
            )
        });
        refused(root, "cannot resolve", || {
            revert(&repo, PickArgs { rev: "nope".into() })
        });
    }
}
