use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use anyhow::{Context, bail};
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    index::entry::{Flags, Mode, Stage, Stat},
    refs::{
        FullName, Target,
        transaction::{PreviousValue, RefEdit},
    },
    worktree::stack::state::attributes,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ChangeKind, index_entry_kind, pathspec_search, resolve_commit, resolve_tree,
    staging::{Version, filemode, replace_entries, tree_versions, worktree_blob},
    write::{
        branch_ref, checked_out_at, committer, ensure_idle, log_head_move, require_worktree,
        update_index,
    },
};

/// Files listed per call; the `*_count` fields always have the full number.
const MAX_LISTED: usize = 500;
/// Blocked paths named in a refusal; the rest are only counted.
const MAX_BLOCKED_NAMED: usize = 10;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RestoreArgs {
    /// Paths or pathspec patterns to restore, relative to the repository root, e.g. `["src/main.rs"]`,
    /// `["src/"]`, or `["."]` for everything.
    pub paths: Vec<String>,
    /// Where the content comes from: `index` (the staged version, the default for the working tree) or
    /// a revision such as `HEAD` or `main~2` (the default with `staged` is `HEAD`). With a revision,
    /// tracked files it doesn't have are deleted, as `git restore --source` does.
    pub source: Option<String>,
    /// Restore index entries from `source`, i.e. unstage (default false).
    #[serde(default)]
    pub staged: bool,
    /// Restore working-tree files (default true, or false when `staged` is set, as with
    /// `git restore --staged`).
    pub worktree: Option<bool>,
    /// Only report what would change (default true). Set `false` to overwrite the files.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RestoredFile {
    /// Path relative to the repository root.
    pub path: String,
    /// How the working-tree file changes (`added` when recreated, `deleted` when removed); `null`
    /// when it's left alone.
    pub worktree: Option<ChangeKind>,
    /// How the index entry changes; `null` when it's left alone.
    pub index: Option<ChangeKind>,
    /// True when content that isn't committed in HEAD (or kept in the index) is overwritten, so it
    /// can't be recovered afterwards.
    pub discards_changes: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RestoreResult {
    /// Files that change, ordered by path; at most 500.
    pub files: Vec<RestoredFile>,
    /// Number of files that change, including any beyond the listed 500.
    pub files_count: usize,
    /// True when any file's `discards_changes` is set.
    pub discards_changes: bool,
    /// Whether this was a dry run; if so, nothing was changed.
    pub dry_run: bool,
}

/// Restore files in the working tree and/or index from the index or a revision, like `git restore`.
pub fn restore(repo: &gix::Repository, args: RestoreArgs) -> anyhow::Result<RestoreResult> {
    let workdir = require_worktree(repo, "restore files")?.to_owned();
    ensure_idle(repo, "restore files")?;
    if args.paths.is_empty() {
        bail!("pass the `paths` to restore, e.g. [\"src/main.rs\"], or [\".\"] for everything");
    }
    let worktree = args.worktree.unwrap_or(!args.staged);
    if !worktree && !args.staged {
        bail!("nothing to restore: set `worktree` or `staged` (or both)");
    }
    let source = args
        .source
        .clone()
        .unwrap_or_else(|| if args.staged { "HEAD" } else { "index" }.into());
    if args.staged && source == "index" {
        bail!(
            "`staged` restores the index from a revision, so `source` can't be `index`; use e.g. `HEAD`"
        );
    }
    let request = RestoreRequest {
        paths: &args.paths,
        source: &source,
        staged: args.staged,
        worktree,
        overwrite_conflicts: false,
    };
    let plan = if args.dry_run {
        plan_restore(repo, &*repo.index_or_empty()?, &workdir, &request)?
    } else {
        update_index(repo, |index| {
            let plan = plan_restore(repo, index, &workdir, &request)?;
            apply_restore(repo, index, &workdir, &plan)?;
            Ok(plan)
        })?
    };
    Ok(summarize(plan, args.dry_run))
}

/// The result for a restore plan: the files listed (at most 500), counted and flagged.
pub fn summarize(plan: Vec<Restore>, dry_run: bool) -> RestoreResult {
    let discards_changes = plan.iter().any(|r| r.discards_changes);
    let files_count = plan.len();
    let files = plan
        .into_iter()
        .take(MAX_LISTED)
        .map(|r| RestoredFile {
            path: r.path.to_string(),
            worktree: r.worktree,
            index: r.index,
            discards_changes: r.discards_changes,
        })
        .collect();
    RestoreResult {
        files,
        files_count,
        discards_changes,
        dry_run,
    }
}

/// The working-tree files a restore plan rewrites or deletes, as switching reports them.
pub fn updated_files(plan: &[Restore]) -> Vec<UpdatedFile> {
    plan.iter()
        .filter_map(|r| {
            Some(UpdatedFile {
                path: r.path.to_string(),
                change: r.worktree?,
            })
        })
        .collect()
}

/// What to restore, from where, and into which of the index and working tree.
pub struct RestoreRequest<'a> {
    pub paths: &'a [String],
    pub source: &'a str,
    pub staged: bool,
    pub worktree: bool,
    /// Replace conflict stages instead of refusing, as `git reset --hard` does.
    pub overwrite_conflicts: bool,
}

/// One path `restore` changes.
pub struct Restore {
    path: BString,
    /// The source's version; `None` when the source doesn't have the path, so it's removed.
    target: Option<Version>,
    worktree: Option<ChangeKind>,
    index: Option<ChangeKind>,
    discards_changes: bool,
}

pub fn plan_restore(
    repo: &gix::Repository,
    index: &gix::index::State,
    workdir: &Path,
    request: &RestoreRequest<'_>,
) -> anyhow::Result<Vec<Restore>> {
    let staged = staged_versions(index);
    let source = if request.source == "index" {
        staged.clone()
    } else {
        tree_versions(repo, resolve_tree(repo, Some(request.source))?.id)?
    };
    let mut spec = pathspec_search(repo, request.paths, index)?.context("`paths` is not empty")?;
    let matched: BTreeSet<BString> = source
        .keys()
        .cloned()
        .chain(index.entries().iter().map(|e| e.path(index).to_owned()))
        .filter(|path| spec.is_included(path.as_bstr(), Some(false)))
        .collect();
    if matched.is_empty() {
        let place = if request.source == "index" {
            "the index".to_owned()
        } else {
            format!("`{}` or the index", request.source)
        };
        bail!("`paths` match nothing in {place}; check them with status or list_tree");
    }
    let conflicted = conflicted_paths(index, &matched);
    if !conflicted.is_empty() && !request.overwrite_conflicts {
        bail!(
            "{} unresolved merge conflicts; resolve and stage them first (`conflicts` lists them)",
            name_some(
                &conflicted
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                "has",
                "have"
            )
        );
    }
    let head = tree_versions(repo, repo.head_tree_id_or_empty()?.detach())?;
    let disk = if request.worktree {
        disk_states(repo, index, workdir, &matched)?
    } else {
        BTreeMap::new()
    };

    let mut plan = Vec::new();
    for path in matched {
        let target = source.get(&path).copied();
        let in_index = staged.get(&path).copied();
        let on_disk = disk.get(&path).copied().unwrap_or(OnDisk::Missing);
        // A submodule's checkout is its own repository: like git, only its gitlink in the index changes.
        let gitlink = [target, in_index]
            .iter()
            .flatten()
            .any(|(_, mode)| *mode == Mode::COMMIT);
        let worktree = (request.worktree && !gitlink)
            .then(|| change(on_disk.version(), target))
            .flatten();
        if request.worktree && !gitlink && on_disk == OnDisk::Directory && target.is_some() {
            bail!(
                "`{path}` is a directory in the working tree; move it away before restoring the file"
            );
        }
        let index_change = request.staged.then(|| change(in_index, target)).flatten();
        if worktree.is_none() && index_change.is_none() {
            continue;
        }
        let saved = |version: Option<Version>| {
            version.is_none() || version == target || version == head.get(&path).copied()
        };
        // Overwritten content survives if HEAD has it, or the index keeps it.
        let worktree_lost = worktree.is_some()
            && !saved(on_disk.version())
            && (request.staged || on_disk.version() != in_index);
        let index_lost = index_change.is_some()
            && !saved(in_index)
            && (request.worktree || in_index != on_disk.version());
        plan.push(Restore {
            path,
            target,
            worktree,
            index: index_change,
            discards_changes: worktree_lost || index_lost,
        });
    }
    Ok(plan)
}

pub fn apply_restore(
    repo: &gix::Repository,
    index: &mut gix::index::State,
    workdir: &Path,
    plan: &[Restore],
) -> anyhow::Result<()> {
    let writes: Vec<_> = plan.iter().filter(|r| r.worktree.is_some()).collect();
    ensure_inside_worktree(workdir, writes.iter().map(|r| r.path.as_bstr()))?;
    for r in writes.iter().filter(|r| r.target.is_none()) {
        remove_from_worktree(workdir, r.path.as_bstr())?;
    }
    let files: Vec<_> = writes
        .iter()
        .filter_map(|r| r.target.map(|v| (r.path.clone(), v)))
        .collect();
    let stats = write_to_worktree(repo, workdir, &files)?;

    let staged = staged_versions(index);
    let mut replacements = Vec::new();
    for r in plan {
        // The index entry after this call: the target when restoring the index, otherwise unchanged.
        let entry = if r.index.is_some() {
            r.target
        } else {
            staged.get(&r.path).copied()
        };
        let Some(entry) = entry else {
            if r.index.is_some() {
                replacements.push((r.path.clone(), None));
            }
            continue;
        };
        // Reuse the fresh stat when the file on disk now holds what the index records.
        let stat = match stats.get(&r.path) {
            Some(stat) if Some(entry) == r.target => *stat,
            _ if r.index.is_none() => continue,
            _ => Stat::default(),
        };
        replacements.push((r.path.clone(), Some((entry.0, entry.1, stat))));
    }
    replace_entries(index, replacements);
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SwitchArgs {
    /// Branch to switch to, e.g. `feature`. With `create`, the name of the new branch; with `detach`,
    /// any revision (tag, SHA, `HEAD~2`) to check out without a branch.
    pub target: String,
    /// Create `target` as a new branch at `start_point` and switch to it, like `git switch -c`
    /// (default false).
    #[serde(default)]
    pub create: bool,
    /// Where a new branch starts (default `HEAD`). Only with `create`.
    pub start_point: Option<String>,
    /// Check out `target` with a detached HEAD, like `git switch --detach` (default false).
    #[serde(default)]
    pub detach: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UpdatedFile {
    /// Path relative to the repository root.
    pub path: String,
    /// How the file changed between the old and the new commit.
    pub change: ChangeKind,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SwitchResult {
    /// Short name of the branch HEAD is on now; `null` for a detached HEAD.
    pub branch: Option<String>,
    /// Full SHA of the commit now checked out.
    pub head: String,
    /// Where HEAD was before: a branch name, or a full SHA when it was detached. Switch back to it to undo.
    pub previous: String,
    /// True when `create` made the branch.
    pub created: bool,
    /// Files the switch updated in the working tree and index, ordered by path; at most 500.
    /// Local changes to other files are carried over untouched.
    pub updated: Vec<UpdatedFile>,
    /// Number of updated files, including any beyond the listed 500.
    pub updated_count: usize,
}

/// Switch HEAD to a branch (or detach it at a commit), updating the working tree and index like
/// `git switch`. Refuses instead of overwriting local changes.
pub fn switch(repo: &gix::Repository, args: SwitchArgs) -> anyhow::Result<SwitchResult> {
    let workdir = require_worktree(repo, "switch")?.to_owned();
    ensure_idle(repo, "switch")?;
    if args.create && args.detach {
        bail!("`create` and `detach` can't be combined");
    }
    if args.start_point.is_some() && !args.create {
        bail!("`start_point` only applies with `create: true`");
    }
    committer(repo)?;
    let from = HeadState::read(repo)?;
    let to = Destination::resolve(repo, &args)?;
    if let Some(name) = &to.branch
        && !args.create
    {
        if from.branch.as_ref() == Some(name) {
            return Ok(SwitchResult {
                branch: Some(name.shorten().to_string()),
                head: to.commit.to_string(),
                previous: from.label,
                created: false,
                updated: Vec::new(),
                updated_count: 0,
            });
        }
        if let Some(path) = checked_out_at(repo, name.as_ref(), true)? {
            bail!(
                "branch `{}` is already checked out at {}; switch there, or use `detach: true`",
                name.shorten(),
                path.display()
            );
        }
    }

    let old_tree = match from.commit {
        Some(id) => repo.find_commit(id)?.tree_id()?.detach(),
        None => ObjectId::empty_tree(repo.object_hash()),
    };
    let new_tree = repo.find_commit(to.commit)?.tree_id()?.detach();
    let changes = tree_changes(repo, old_tree, new_tree)?;
    let mut updated = update_index(repo, |index| {
        let action = format!("switching to `{}`", args.target);
        let plan = plan_switch(repo, index, &workdir, &changes, &action)?;
        apply_switch(repo, index, &workdir, plan)
    })?;

    let message = format!("checkout: moving from {} to {}", from.label, args.target);
    let mut edits = Vec::new();
    if args.create
        && let Some(name) = &to.branch
    {
        let start = args.start_point.as_deref().unwrap_or("HEAD");
        edits.push(RefEdit::update(
            name.clone(),
            to.commit,
            PreviousValue::MustNotExist,
            format!("branch: Created from {start}"),
        ));
    }
    let new_head = match &to.branch {
        Some(name) => Target::Symbolic(name.clone()),
        None => Target::Object(to.commit),
    };
    edits.push(
        RefEdit::update(
            "HEAD".try_into()?,
            new_head,
            PreviousValue::MustExistAndMatch(from.target.clone()),
            message.as_str(),
        )
        .with_deref(false),
    );
    repo.edit_references(edits).context(
        "the working tree and index were updated, but HEAD couldn't be moved (it changed meanwhile?); \
         check with status and switch again",
    )?;
    if to.branch.is_some() {
        // A detached HEAD's update was logged by the transaction; a symbolic one isn't.
        log_head_move(repo, from.commit, to.commit, &message)?;
    }

    updated.sort_by(|a, b| a.path.cmp(&b.path));
    let updated_count = updated.len();
    updated.truncate(MAX_LISTED);
    Ok(SwitchResult {
        branch: to.branch.map(|name| name.shorten().to_string()),
        head: to.commit.to_string(),
        previous: from.label,
        created: args.create,
        updated,
        updated_count,
    })
}

/// Where HEAD is before switching.
struct HeadState {
    /// What HEAD points to, for a compare-and-swap update.
    target: Target,
    branch: Option<FullName>,
    commit: Option<ObjectId>,
    /// The branch's short name, or the commit SHA when detached, for messages and the result.
    label: String,
}

impl HeadState {
    fn read(repo: &gix::Repository) -> anyhow::Result<Self> {
        let head = repo.head()?;
        let branch = head.referent_name().map(ToOwned::to_owned);
        let commit = head.id().map(|id| id.detach());
        let (target, label) = match (&branch, commit) {
            (Some(name), _) => (Target::Symbolic(name.clone()), name.shorten().to_string()),
            (None, Some(id)) => (Target::Object(id), id.to_string()),
            (None, None) => bail!("HEAD is neither on a branch nor at a commit"),
        };
        Ok(Self {
            target,
            branch,
            commit,
            label,
        })
    }
}

/// Where HEAD goes.
struct Destination {
    /// `None` for a detached HEAD.
    branch: Option<FullName>,
    commit: ObjectId,
}

impl Destination {
    fn resolve(repo: &gix::Repository, args: &SwitchArgs) -> anyhow::Result<Self> {
        if args.detach {
            return Ok(Self {
                branch: None,
                commit: resolve_commit(repo, Some(&args.target))?.id,
            });
        }
        let name = branch_ref(&args.target)?;
        let existing = repo.try_find_reference(name.as_bstr())?;
        if args.create {
            if existing.is_some() {
                bail!(
                    "branch `{}` already exists; switch to it without `create`, or pick another name",
                    name.shorten()
                );
            }
            return Ok(Self {
                commit: resolve_commit(repo, args.start_point.as_deref())?.id,
                branch: Some(name),
            });
        }
        let Some(mut reference) = existing else {
            bail!(
                "there is no branch `{}`; set `create: true` to create it, or `detach: true` to check \
                 out a tag or commit",
                args.target
            );
        };
        Ok(Self {
            commit: reference.peel_to_commit()?.id,
            branch: Some(name),
        })
    }
}

/// Paths whose version differs between two trees: `(old, new)`, `None` where a tree lacks the path.
pub type TreeChanges = BTreeMap<BString, (Option<Version>, Option<Version>)>;

/// The [`TreeChanges`] from `old` to `new`.
pub fn tree_changes(
    repo: &gix::Repository,
    old: ObjectId,
    new: ObjectId,
) -> anyhow::Result<TreeChanges> {
    let old = tree_versions(repo, old)?;
    let new = tree_versions(repo, new)?;
    let paths: BTreeSet<&BString> = old.keys().chain(new.keys()).collect();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let pair = (old.get(path).copied(), new.get(path).copied());
            (pair.0 != pair.1).then(|| (path.clone(), pair))
        })
        .collect())
}

/// A path the switch rewrites: its version before and after.
pub struct Step {
    pub path: BString,
    pub old: Option<Version>,
    pub new: Option<Version>,
}

/// Check every changed path against local changes, git's two-way rule: a path the switch rewrites
/// must be clean in the index and working tree, and nothing untracked may be in the way.
pub fn plan_switch(
    repo: &gix::Repository,
    index: &gix::index::State,
    workdir: &Path,
    changes: &TreeChanges,
    action: &str,
) -> anyhow::Result<Vec<Step>> {
    let staged = staged_versions(index);
    let paths: BTreeSet<BString> = changes.keys().cloned().collect();
    let conflicted = conflicted_paths(index, &paths);
    let disk = disk_states(repo, index, workdir, &paths)?;
    let deleted: BTreeSet<&BStr> = changes
        .iter()
        .filter(|(_, (_, new))| new.is_none())
        .map(|(path, _)| path.as_bstr())
        .collect();
    let root = workdir.canonicalize()?;

    let mut blocked: Vec<String> = Vec::new();
    let mut steps = Vec::new();
    for (path, &(old, new)) in changes {
        let in_index = staged.get(path).copied();
        if conflicted.contains(path) {
            blocked.push(format!("{path} (unresolved conflict)"));
            continue;
        }
        // The index already has the new version: keep the index and working tree as they are.
        if in_index == new {
            continue;
        }
        if in_index != old {
            blocked.push(format!("{path} (staged changes)"));
            continue;
        }
        if escapes(&root, &workdir.join(gix::path::from_bstr(path.as_bstr())?)) {
            blocked.push(format!(
                "{path} (leads outside the repository through a symlink)"
            ));
            continue;
        }
        let gitlink = [old, new]
            .iter()
            .flatten()
            .any(|(_, mode)| *mode == Mode::COMMIT);
        let on_disk = disk.get(path).copied().unwrap_or(OnDisk::Missing);
        if !gitlink {
            let clean = match (in_index, on_disk) {
                (Some(version), OnDisk::File(found)) => found == version,
                (Some(_), OnDisk::Missing) => new.is_none(),
                (Some(_), OnDisk::Directory) => false,
                (None, OnDisk::Missing) => true,
                (None, OnDisk::Directory) => {
                    only_deleted_files_below(workdir, path.as_bstr(), &deleted)?
                }
                (None, OnDisk::File(_)) => false,
            };
            if !clean {
                let why = if in_index.is_some() {
                    "unstaged changes"
                } else {
                    "untracked file in the way"
                };
                blocked.push(format!("{path} ({why})"));
                continue;
            }
        }
        if new.is_some()
            && let Some(parent) = file_in_the_way(workdir, path.as_bstr(), &deleted)
        {
            blocked.push(format!("{path} (`{parent}` is a file in the way)"));
            continue;
        }
        steps.push(Step {
            path: path.clone(),
            old,
            new,
        });
    }
    if !blocked.is_empty() {
        bail!(
            "{action} would overwrite local changes; commit, stash or restore them first: {}",
            name_some(&blocked, "", "")
        );
    }
    Ok(steps)
}

pub fn apply_switch(
    repo: &gix::Repository,
    index: &mut gix::index::State,
    workdir: &Path,
    steps: Vec<Step>,
) -> anyhow::Result<Vec<UpdatedFile>> {
    // Deletions first, so a directory can make way for a file of the same name.
    for step in steps.iter().filter(|s| s.new.is_none()) {
        remove_from_worktree(workdir, step.path.as_bstr())?;
    }
    let mut files = Vec::new();
    for step in &steps {
        match step.new {
            Some((_, Mode::COMMIT)) => {
                // Like `git switch` without `--recurse-submodules`: only an empty directory is made.
                std::fs::create_dir_all(workdir.join(gix::path::from_bstr(step.path.as_bstr())?))?;
            }
            Some(version) => files.push((step.path.clone(), version)),
            None => {}
        }
    }
    let stats = write_to_worktree(repo, workdir, &files)?;
    let mut updated = Vec::new();
    let mut replacements = Vec::new();
    for step in steps {
        let entry = step
            .new
            .map(|(id, mode)| (id, mode, stats.get(&step.path).copied().unwrap_or_default()));
        updated.push(UpdatedFile {
            path: step.path.to_string(),
            change: change(step.old, step.new).context("each step changes its path")?,
        });
        replacements.push((step.path, entry));
    }
    replace_entries(index, replacements);
    Ok(updated)
}

/// What sits at a path in the working tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnDisk {
    Missing,
    Directory,
    /// A file or symlink, hashed the way `git add` would store it.
    File(Version),
}

impl OnDisk {
    fn version(self) -> Option<Version> {
        match self {
            Self::File(version) => Some(version),
            Self::Missing | Self::Directory => None,
        }
    }
}

/// Hash the working-tree files at `paths` without writing any objects.
fn disk_states(
    repo: &gix::Repository,
    index: &gix::index::State,
    workdir: &Path,
    paths: &BTreeSet<BString>,
) -> anyhow::Result<BTreeMap<BString, OnDisk>> {
    let memory = repo.clone().with_object_memory();
    let (mut pipeline, _) = memory.filter_pipeline(None)?;
    let filemode = filemode(repo);
    let staged = staged_versions(index);
    let mut states = BTreeMap::new();
    for path in paths {
        let full = workdir.join(gix::path::from_bstr(path.as_bstr())?);
        let state = match std::fs::symlink_metadata(&full) {
            // `a/b` can't exist while `a` is a file.
            Err(err) if is_absent(&err) => OnDisk::Missing,
            Err(err) => return Err(err).with_context(|| format!("cannot read `{path}`")),
            Ok(meta) if meta.is_dir() => OnDisk::Directory,
            Ok(_) => {
                let previous = staged.get(path).map(|(_, mode)| *mode);
                match worktree_blob(
                    &memory,
                    &mut pipeline,
                    index,
                    workdir,
                    path.as_bstr(),
                    previous,
                    filemode,
                )? {
                    Some((id, mode, _)) => OnDisk::File((id, mode)),
                    None => OnDisk::Missing,
                }
            }
        };
        states.insert(path.clone(), state);
    }
    Ok(states)
}

/// Write `files` into the working tree as `git checkout` does (smudge filters, line endings,
/// symlinks, executable bit), returning each file's new stat for the index.
pub fn write_to_worktree(
    repo: &gix::Repository,
    workdir: &Path,
    files: &[(BString, Version)],
) -> anyhow::Result<BTreeMap<BString, Stat>> {
    if files.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut checkout = gix::index::State::new(repo.object_hash());
    for (path, (id, mode)) in files {
        checkout.dangerously_push_entry(
            Stat::default(),
            *id,
            Flags::empty(),
            *mode,
            path.as_bstr(),
        );
    }
    checkout.sort_entries();
    let mut options = repo.checkout_options(attributes::Source::WorktreeThenIdMapping)?;
    options.overwrite_existing = true;
    options.destination_is_initially_empty = false;
    let outcome = gix::worktree::state::checkout(
        &mut checkout,
        workdir,
        repo.objects.clone().into_arc()?,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &AtomicBool::new(false),
        options,
    )?;
    let failed: Vec<String> = outcome
        .errors
        .iter()
        .map(|e| format!("{} ({})", e.path, e.error))
        .chain(
            outcome
                .collisions
                .iter()
                .map(|c| format!("{} ({:?})", c.path, c.error_kind)),
        )
        .collect();
    if !failed.is_empty() {
        bail!(
            "some files couldn't be written: {}",
            name_some(&failed, "", "")
        );
    }
    Ok(checkout
        .entries()
        .iter()
        .map(|entry| (entry.path(&checkout).to_owned(), entry.stat))
        .collect())
}

/// Delete a file (or an empty submodule directory) and any directories that become empty, as git does.
pub fn remove_from_worktree(workdir: &Path, path: &BStr) -> anyhow::Result<()> {
    let full = workdir.join(gix::path::from_bstr(path)?);
    match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.is_dir() => {
            // A submodule directory goes only if it's empty.
            std::fs::remove_dir(&full).ok();
        }
        Ok(_) => std::fs::remove_file(&full).with_context(|| format!("cannot delete `{path}`"))?,
        Err(err) if is_absent(&err) => {}
        Err(err) => return Err(err).with_context(|| format!("cannot read `{path}`")),
    }
    let mut dir = full.parent();
    while let Some(parent) = dir {
        if parent == workdir || std::fs::remove_dir(parent).is_err() {
            break;
        }
        dir = parent.parent();
    }
    Ok(())
}

/// Whether a lookup failed because nothing is there, including `a/b` while `a` is a file.
fn is_absent(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Refuse to write anywhere a symlinked directory would lead outside the working tree.
fn ensure_inside_worktree<'a>(
    workdir: &Path,
    paths: impl IntoIterator<Item = &'a BStr>,
) -> anyhow::Result<()> {
    let root = workdir.canonicalize()?;
    for path in paths {
        if escapes(&root, &workdir.join(gix::path::from_bstr(path)?)) {
            bail!(
                "`{path}` leads outside the repository through a symlinked directory; refusing to write it"
            );
        }
    }
    Ok(())
}

/// Whether the nearest existing ancestor of `full` resolves outside `root`.
fn escapes(root: &Path, full: &Path) -> bool {
    let mut dir = full.parent();
    while let Some(parent) = dir {
        if std::fs::symlink_metadata(parent).is_ok() {
            return parent
                .canonicalize()
                .map_or(true, |real| !real.starts_with(root));
        }
        dir = parent.parent();
    }
    false
}

/// A leading component of `path` that exists as a file (not a directory) and isn't being deleted.
fn file_in_the_way(workdir: &Path, path: &BStr, deleted: &BTreeSet<&BStr>) -> Option<String> {
    path.char_indices()
        .filter(|(_, _, c)| *c == '/')
        .map(|(start, _, _)| &path[..start])
        .find(|parent| {
            let full = gix::path::from_bstr(*parent).map(|p| workdir.join(p));
            full.is_ok_and(|full| std::fs::symlink_metadata(full).is_ok_and(|m| !m.is_dir()))
                && !deleted.contains(parent)
        })
        .map(ToString::to_string)
}

/// Whether every file below the directory at `path` is a tracked file this switch deletes.
fn only_deleted_files_below(
    workdir: &Path,
    path: &BStr,
    deleted: &BTreeSet<&BStr>,
) -> anyhow::Result<bool> {
    let mut pending = vec![PathBuf::from(gix::path::from_bstr(path)?.as_ref())];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(workdir.join(&dir))? {
            let entry = entry?;
            let rela = dir.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                pending.push(rela);
            } else if !deleted.contains(gix::path::into_bstr(rela.as_path())?.as_ref()) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Stage-0 entries of the index, by path.
pub fn staged_versions(index: &gix::index::State) -> BTreeMap<BString, Version> {
    index
        .entries()
        .iter()
        .filter(|e| e.stage() == Stage::Unconflicted)
        .map(|e| (e.path(index).to_owned(), (e.id, e.mode)))
        .collect()
}

/// The paths among `among` with unresolved conflict stages in the index.
fn conflicted_paths(index: &gix::index::State, among: &BTreeSet<BString>) -> BTreeSet<BString> {
    index
        .entries()
        .iter()
        .filter(|e| e.stage() != Stage::Unconflicted && among.contains(e.path(index)))
        .map(|e| e.path(index).to_owned())
        .collect()
}

/// How a path changes from `from` to `to`; `None` when it doesn't.
fn change(from: Option<Version>, to: Option<Version>) -> Option<ChangeKind> {
    match (from, to) {
        (a, b) if a == b => None,
        (None, Some(_)) => Some(ChangeKind::Added),
        (Some(_), None) => Some(ChangeKind::Deleted),
        (Some((_, old)), Some((_, new))) => Some(ChangeKind::of_modification(
            index_entry_kind(old),
            index_entry_kind(new),
        )),
        (None, None) => None,
    }
}

/// `items` joined for a message, naming at most a few and counting the rest. With `singular` and
/// `plural` set, a matching verb follows (`a.txt has`, `a.txt and b.txt have`).
fn name_some(items: &[String], singular: &str, plural: &str) -> String {
    let mut text = items
        .iter()
        .take(MAX_BLOCKED_NAMED)
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > MAX_BLOCKED_NAMED {
        text.push_str(&format!(" and {} more", items.len() - MAX_BLOCKED_NAMED));
    }
    let verb = if items.len() == 1 { singular } else { plural };
    if verb.is_empty() {
        text
    } else {
        format!("{text} {verb}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{
        assert_unchanged, bare_repo, fixture_repo, git_stdout, ls_files, open, porcelain, run_git,
        snapshot, try_git, twins, worktree_files, write,
    };

    fn switch_to(repo: &gix::Repository, target: &str) -> anyhow::Result<SwitchResult> {
        switch(
            repo,
            SwitchArgs {
                target: target.into(),
                create: false,
                start_point: None,
                detach: false,
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

    /// Compare everything a checkout touches: files, index, status, HEAD and its last reflog entry.
    fn assert_same_checkout(ours: &Path, theirs: &Path) {
        assert_eq!(worktree_files(ours), worktree_files(theirs));
        assert_eq!(ls_files(ours), ls_files(theirs));
        assert_eq!(porcelain(ours), porcelain(theirs));
        let head = |root: &Path| git_stdout(root, &["rev-parse", "--symbolic-full-name", "HEAD"]);
        assert_eq!(head(ours), head(theirs));
        let last =
            |root: &Path| git_stdout(root, &["reflog", "show", "-1", "--format=%gs", "HEAD"]);
        assert_eq!(last(ours), last(theirs));
    }

    /// `main` and `other` differ by an edit, a deletion, a file that becomes a directory, new nested
    /// files, an executable, a symlink and a unicode path; `same.txt` is identical on both.
    fn branches(root: &Path) {
        write(root, "d.txt", "delete me\n");
        write(root, "f", "a file\n");
        write(root, "same.txt", "same\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "main files"]);
        run_git(root, &["checkout", "-q", "-b", "other"]);
        write(root, "a.txt", "other version\n");
        run_git(root, &["rm", "-q", "d.txt", "f"]);
        write(root, "f/inner.txt", "now a directory\n");
        write(root, "new/deep/x.txt", "new\n");
        write(root, "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(
            root.join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        write(root, "dïr/ünï.txt", "unicode\n");
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-q", "-m", "other changes"]);
        run_git(root, &["checkout", "-q", "main"]);
    }

    #[test]
    fn switching_both_ways_matches_git() {
        let [(ours, repo), (theirs, _)] = twins(branches);
        let res = switch_to(&repo, "other").unwrap();
        run_git(theirs.path(), &["switch", "-q", "other"]);
        assert_same_checkout(ours.path(), theirs.path());
        assert_eq!(res.branch.as_deref(), Some("other"));
        assert_eq!(res.previous, "main");
        assert_eq!(
            res.head,
            git_stdout(ours.path(), &["rev-parse", "other"]).trim()
        );
        let changes: Vec<_> = res
            .updated
            .iter()
            .map(|u| (u.path.as_str(), u.change))
            .collect();
        assert_eq!(
            changes,
            [
                ("a.txt", ChangeKind::Modified),
                ("d.txt", ChangeKind::Deleted),
                ("dïr/ünï.txt", ChangeKind::Added),
                ("f", ChangeKind::Deleted),
                ("f/inner.txt", ChangeKind::Added),
                ("link", ChangeKind::Added),
                ("new/deep/x.txt", ChangeKind::Added),
                ("run.sh", ChangeKind::Added),
            ]
        );

        switch_to(&repo, "main").unwrap();
        run_git(theirs.path(), &["switch", "-q", "main"]);
        assert_same_checkout(ours.path(), theirs.path());
        assert!(
            !ours.path().join("new").exists(),
            "emptied directories are removed"
        );
    }

    #[test]
    fn local_changes_elsewhere_are_carried_over() {
        let local = |root: &Path| {
            branches(root);
            write(root, "same.txt", "edited locally\n");
            write(root, "local.txt", "staged locally\n");
            run_git(root, &["add", "local.txt"]);
            write(root, "scratch.txt", "untracked\n");
            // Already staged exactly as `other` has it: kept as is.
            write(root, "a.txt", "other version\n");
            run_git(root, &["add", "a.txt"]);
        };
        let [(ours, repo), (theirs, _)] = twins(local);
        switch_to(&repo, "other").unwrap();
        run_git(theirs.path(), &["switch", "-q", "other"]);
        assert_same_checkout(ours.path(), theirs.path());
    }

    #[test]
    fn switch_refuses_to_overwrite_local_changes() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        branches(root);
        let repo = open(root);
        write(root, "a.txt", "unstaged edit\n");
        refused(root, "a.txt (unstaged changes)", || {
            switch_to(&repo, "other")
        });
        run_git(root, &["add", "a.txt"]);
        refused(root, "a.txt (staged changes)", || switch_to(&repo, "other"));
        run_git(root, &["reset", "-q", "--hard"]);
        write(root, "new/deep/x.txt", "untracked\n");
        refused(root, "new/deep/x.txt (untracked file in the way)", || {
            switch_to(&repo, "other")
        });
        std::fs::remove_dir_all(root.join("new")).unwrap();
        write(root, "d.txt", "edited before deletion\n");
        refused(root, "d.txt (unstaged changes)", || {
            switch_to(&repo, "other")
        });
    }

    #[test]
    fn switch_blocks_whatever_is_in_the_way() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        branches(root);
        let repo = open(root);
        write(root, "new", "an untracked file where a directory must go\n");
        refused(root, "`new` is a file in the way", || {
            switch_to(&repo, "other")
        });
        std::fs::remove_file(root.join("new")).unwrap();

        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("new")).unwrap();
        refused(root, "outside the repository", || switch_to(&repo, "other"));
        std::fs::remove_file(root.join("new")).unwrap();

        std::fs::remove_file(root.join("a.txt")).unwrap();
        refused(root, "a.txt (unstaged changes)", || {
            switch_to(&repo, "other")
        });
        run_git(root, &["checkout", "--", "a.txt"]);

        switch_to(&repo, "other").unwrap();
        write(
            root,
            "f/extra.txt",
            "untracked inside a directory that must become a file\n",
        );
        refused(root, "f (untracked file in the way)", || {
            switch_to(&repo, "main")
        });
    }

    #[test]
    fn deleting_an_already_deleted_file_is_fine() {
        let gone = |root: &Path| {
            branches(root);
            std::fs::remove_file(root.join("d.txt")).unwrap();
        };
        let [(ours, repo), (theirs, _)] = twins(gone);
        switch_to(&repo, "other").unwrap();
        run_git(theirs.path(), &["switch", "-q", "other"]);
        assert_same_checkout(ours.path(), theirs.path());
    }

    #[test]
    fn conflict_stages_block_a_switch() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "-b", "side", "main"]);
        write(root, "a.txt", "ours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        std::fs::remove_file(root.join(".git/MERGE_HEAD")).unwrap();
        let repo = open(root);
        refused(root, "a.txt (unresolved conflict)", || {
            switch_to(&repo, "main")
        });
    }

    #[test]
    fn submodules_switch_like_git() {
        let (sub, _) = fixture_repo();
        let url = sub.path().to_str().unwrap().to_owned();
        let with_submodule = move |root: &Path| {
            run_git(root, &["checkout", "-q", "-b", "vendored"]);
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
            run_git(root, &["checkout", "-q", "main"]);
            std::fs::remove_dir_all(root.join("sub")).ok();
        };
        let [(ours, repo), (theirs, _)] = twins(with_submodule);
        switch_to(&repo, "vendored").unwrap();
        run_git(theirs.path(), &["switch", "-q", "vendored"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert!(
            ours.path().join("sub").is_dir(),
            "an empty directory stands in for the submodule"
        );
        switch_to(&repo, "main").unwrap();
        run_git(theirs.path(), &["switch", "-q", "main"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(
            ours.path().join("sub").exists(),
            theirs.path().join("sub").exists()
        );
    }

    #[test]
    fn create_and_detach_match_git() {
        let [(ours, repo), (theirs, _)] = twins(branches);
        let res = switch(
            &repo,
            SwitchArgs {
                target: "topic".into(),
                create: true,
                start_point: Some("other".into()),
                detach: false,
            },
        )
        .unwrap();
        run_git(theirs.path(), &["switch", "-q", "-c", "topic", "other"]);
        assert_same_checkout(ours.path(), theirs.path());
        assert!(res.created);
        let log =
            |root: &Path| git_stdout(root, &["reflog", "show", "-1", "--format=%gs", "topic"]);
        assert_eq!(log(ours.path()), log(theirs.path()));

        let detached = switch(
            &repo,
            SwitchArgs {
                target: "v1".into(),
                create: false,
                start_point: None,
                detach: true,
            },
        )
        .unwrap();
        run_git(theirs.path(), &["switch", "-q", "--detach", "v1"]);
        assert_same_checkout(ours.path(), theirs.path());
        assert!(detached.branch.is_none());
        assert_eq!(detached.previous, "topic");

        let back = switch_to(&repo, "main").unwrap();
        assert_eq!(
            back.previous, detached.head,
            "a detached HEAD is named by its SHA"
        );
    }

    #[test]
    fn switch_argument_errors_change_nothing() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        branches(root);
        let repo = open(root);
        let same = switch_to(&repo, "main").unwrap();
        assert!(same.updated.is_empty() && same.previous == "main");
        refused(root, "no branch `nope`", || switch_to(&repo, "nope"));
        refused(root, "no branch `v1`", || switch_to(&repo, "v1"));
        let args = |create, detach, start: Option<&str>| SwitchArgs {
            target: "x".into(),
            create,
            start_point: start.map(Into::into),
            detach,
        };
        refused(root, "can't be combined", || {
            switch(&repo, args(true, true, None))
        });
        refused(root, "only applies", || {
            switch(&repo, args(false, false, Some("HEAD")))
        });
        refused(root, "already exists", || {
            switch(
                &repo,
                SwitchArgs {
                    target: "other".into(),
                    create: true,
                    start_point: None,
                    detach: false,
                },
            )
        });
        let wt = tempfile::tempdir().unwrap();
        let path = wt.path().join("linked");
        run_git(
            root,
            &["worktree", "add", "-q", path.to_str().unwrap(), "feature"],
        );
        refused(root, "already checked out", || switch_to(&repo, "feature"));

        let (_dir, bare) = bare_repo();
        assert!(
            switch_to(&bare, "main")
                .unwrap_err()
                .to_string()
                .contains("bare")
        );
    }

    #[test]
    fn switch_refuses_during_a_merge() {
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
        refused(root, "in progress", || switch_to(&repo, "feature"));
        refused(root, "in progress", || {
            restore(&repo, restore_args(&["a.txt"], None, false, None, false))
        });
    }

    fn restore_args(
        paths: &[&str],
        source: Option<&str>,
        staged: bool,
        worktree: Option<bool>,
        dry_run: bool,
    ) -> RestoreArgs {
        RestoreArgs {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            source: source.map(Into::into),
            staged,
            worktree,
            dry_run,
        }
    }

    /// Unstaged and staged edits, a deleted file and a new committed file.
    fn edited(root: &Path) {
        write(root, "b.txt", "bee\n");
        write(root, "c.txt", "sea\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "more"]);
        write(root, "a.txt", "unstaged edit\n");
        write(root, "b.txt", "staged edit\n");
        run_git(root, &["add", "b.txt"]);
        std::fs::remove_file(root.join("c.txt")).unwrap();
    }

    #[test]
    fn restore_is_a_dry_run_by_default() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        edited(root);
        let repo = open(root);
        let parsed: RestoreArgs = serde_json::from_str(r#"{"paths":["."]}"#).unwrap();
        assert!(parsed.dry_run && !parsed.staged && parsed.worktree.is_none());
        let before = snapshot(root);
        let res = restore(&repo, parsed).unwrap();
        assert_unchanged(root, &before);
        assert!(res.dry_run && res.discards_changes);
        let files: Vec<_> = res
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.worktree, f.discards_changes))
            .collect();
        assert_eq!(
            files,
            [
                ("a.txt", Some(ChangeKind::Modified), true),
                ("c.txt", Some(ChangeKind::Added), false),
            ],
            "b.txt matches the index, so restoring from the index leaves it"
        );
    }

    /// Paths, source, staged, worktree, and the `git` arguments that do the same.
    type RestoreCase = (
        &'static [&'static str],
        Option<&'static str>,
        bool,
        Option<bool>,
        &'static [&'static str],
    );

    #[test]
    fn restore_variants_match_git_restore() {
        let cases: [RestoreCase; 5] = [
            (&["."], None, false, None, &["restore", "."]),
            (
                &["a.txt", "c.txt"],
                Some("HEAD~1"),
                false,
                None,
                &["restore", "--source=HEAD~1", "a.txt", "c.txt"],
            ),
            (
                &["b.txt"],
                None,
                true,
                None,
                &["restore", "--staged", "b.txt"],
            ),
            (
                &["."],
                Some("HEAD"),
                true,
                Some(true),
                &["restore", "--staged", "--worktree", "--source=HEAD", "."],
            ),
            (
                &["*.txt"],
                Some("HEAD~1"),
                true,
                Some(true),
                &[
                    "restore",
                    "--staged",
                    "--worktree",
                    "--source=HEAD~1",
                    "--",
                    "*.txt",
                ],
            ),
        ];
        for (paths, source, staged, worktree, git) in cases {
            let [(ours, repo), (theirs, _)] = twins(edited);
            restore(&repo, restore_args(paths, source, staged, worktree, false)).unwrap();
            run_git(theirs.path(), git);
            assert_eq!(
                worktree_files(ours.path()),
                worktree_files(theirs.path()),
                "{git:?}"
            );
            assert_eq!(ls_files(ours.path()), ls_files(theirs.path()), "{git:?}");
            assert_eq!(porcelain(ours.path()), porcelain(theirs.path()), "{git:?}");
        }
    }

    #[test]
    fn staged_content_counts_as_saved() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        edited(root);
        let repo = open(root);
        let res = restore(
            &repo,
            restore_args(&["b.txt"], Some("HEAD"), false, None, true),
        )
        .unwrap();
        assert!(
            !res.files[0].discards_changes,
            "the index still has the staged edit"
        );
        let res = restore(
            &repo,
            restore_args(&["b.txt"], Some("HEAD"), true, Some(true), true),
        )
        .unwrap();
        assert!(
            res.files[0].discards_changes,
            "restoring the index too loses the edit"
        );
    }

    #[test]
    fn restore_refusals_change_nothing() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        edited(root);
        let repo = open(root);
        refused(root, "match nothing", || {
            restore(&repo, restore_args(&["nope.txt"], None, false, None, false))
        });
        refused(root, "`paths`", || {
            restore(&repo, restore_args(&[], None, false, None, false))
        });
        refused(root, "can't be `index`", || {
            restore(
                &repo,
                restore_args(&["a.txt"], Some("index"), true, None, false),
            )
        });
        refused(root, "nothing to restore", || {
            restore(
                &repo,
                restore_args(&["a.txt"], None, false, Some(false), false),
            )
        });
        refused(root, "cannot resolve", || {
            restore(
                &repo,
                restore_args(&["a.txt"], Some("nope"), false, None, false),
            )
        });

        // Conflict stages left behind without an operation in progress.
        run_git(root, &["checkout", "-q", "-f", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "ours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        std::fs::remove_file(root.join(".git/MERGE_HEAD")).unwrap();
        let repo = open(root);
        refused(root, "unresolved merge conflicts", || {
            restore(&repo, restore_args(&["a.txt"], None, false, None, false))
        });

        let (_dir, bare) = bare_repo();
        assert!(
            restore(&bare, restore_args(&["a.txt"], None, false, None, true))
                .unwrap_err()
                .to_string()
                .contains("bare")
        );
    }

    #[test]
    fn restoring_everything_leaves_submodule_checkouts_alone() {
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
        restore(
            &repo,
            restore_args(&["."], Some("HEAD"), true, Some(true), false),
        )
        .unwrap();
        run_git(
            theirs.path(),
            &["restore", "--staged", "--worktree", "--source=HEAD", "."],
        );
        assert_eq!(worktree_files(ours.path()), worktree_files(theirs.path()));
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
    }

    /// A commit whose tree has `.GIT/config` (CVE-2014-9390): on a case-insensitive filesystem, checking
    /// it out would overwrite the repository's own config, so gix's path validation must refuse it.
    #[test]
    fn trees_that_write_into_git_are_refused() {
        use crate::tools::testutil::git_output;
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let stdout = |args: &[&str], input: &str| {
            let out = git_output(root, args, input.as_bytes());
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let blob = stdout(
            &["hash-object", "-w", "--stdin"],
            "[core]\n\tpwned = true\n",
        );
        let inner = stdout(&["mktree"], &format!("100644 blob {blob}\tconfig\n"));
        let a = stdout(&["rev-parse", "HEAD:a.txt"], "");
        let tree = stdout(
            &["mktree"],
            &format!("040000 tree {inner}\t.GIT\n100644 blob {a}\ta.txt\n"),
        );
        let commit = stdout(&["commit-tree", &tree, "-p", "HEAD", "-m", "evil"], "");
        run_git(root, &["branch", "evil", &commit]);

        let config = std::fs::read(root.join(".git/config")).unwrap();
        let before = snapshot(root);
        let result = switch_to(&repo, "evil");
        assert!(result.is_err(), "{result:?}");
        assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), config);
        assert_unchanged(root, &before);
    }

    #[test]
    fn writes_through_symlinked_directories_are_refused() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "sub/file.txt", "inside\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "sub"]);
        let outside = tempfile::tempdir().unwrap();
        std::fs::remove_dir_all(root.join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("sub")).unwrap();
        let repo = open(root);
        refused(root, "outside the repository", || {
            restore(
                &repo,
                restore_args(&["sub/file.txt"], None, false, None, false),
            )
        });
        assert!(!outside.path().join("file.txt").exists());
    }
}
