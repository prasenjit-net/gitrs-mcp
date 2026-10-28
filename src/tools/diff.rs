use std::fmt::Write as _;

use anyhow::bail;
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    diff::Rewrites,
    diff::blob::{
        Algorithm, InternedInput, ResourceKind, UnifiedDiff,
        pipeline::{Mode, WorktreeRoots},
        platform::resource::Data,
        unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader},
    },
    object::tree::EntryKind,
    status::{
        UntrackedFiles, index_worktree,
        plumbing::index_as_worktree::{Change, EntryStatus},
        tree_index::TrackRenames,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ChangeKind, byte_budget, index_entry_kind, octal_mode, page_limit, pathspec_patterns,
    pathspec_search, resolve_commit, resolve_tree, short_hex,
};

const DEFAULT_DIFF_LIMIT: usize = 100;
const MAX_DIFF_LIMIT: usize = 1000;
const DEFAULT_CONTEXT_LINES: u32 = 3;

/// How much detail to return per file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiffFormat {
    /// Status plus added/deleted line counts per file, like `git diff --numstat`.
    #[default]
    Stat,
    /// Status and paths only, like `git diff --name-status`. Fastest: file contents aren't read.
    NameStatus,
    /// Line counts plus a unified diff per file, like `git diff`, within `max_bytes`.
    Patch,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffArgs {
    /// Old side. With `to`: the first of two revisions to compare. With `staged`: the revision the
    /// index is compared with (default `HEAD`). Comparing `from` alone with the working tree isn't supported.
    pub from: Option<String>,
    /// New side when comparing two revisions; requires `from`. Cannot be combined with `staged`.
    pub to: Option<String>,
    /// Compare the index (staged changes) with `from` (default `HEAD`), like `git diff --cached`.
    /// With neither `staged` nor `to`, the worktree is compared with the index, like plain `git diff`.
    #[serde(default)]
    pub staged: bool,
    /// Limit to these paths or git pathspec patterns (e.g. `src/`, `*.rs`), relative to the repository root.
    #[serde(default)]
    pub pathspec: Vec<String>,
    /// Detail per file: `stat` (default), `name_status` or `patch`.
    #[serde(default)]
    pub format: DiffFormat,
    /// Lines of unchanged context around each change in patches (default 3).
    pub context_lines: Option<u32>,
    /// Byte budget for all patch text in this response (default 65536, max 1048576). Files whose patch
    /// doesn't fit get `patch: null`; page on with `skip` to fetch them.
    pub max_bytes: Option<usize>,
    /// Maximum files to return (default 100, max 1000). `totals` always cover every file.
    pub limit: Option<usize>,
    /// Number of files to skip first, for pagination.
    pub skip: Option<usize>,
}

/// Which two states were compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiffMode {
    /// Index (old) against the working tree (new): unstaged changes.
    Worktree,
    /// A revision's tree (old) against the index (new): staged changes.
    Staged,
    /// Two revisions' trees.
    Revisions,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiffFile {
    /// Path relative to the repository root; the new path for renames and copies.
    pub path: String,
    /// Previous path for `renamed` and `copied`; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// How the file changed.
    pub status: ChangeKind,
    /// Lines added; `null` for binary files and with `name_status`.
    pub additions: Option<u32>,
    /// Lines deleted; `null` for binary files and with `name_status`.
    pub deletions: Option<u32>,
    /// True if either side is binary. Binary files get no line counts and only a one-line patch.
    pub binary: bool,
    /// Unified diff starting at `diff --git`, ready for `git apply`. Only with format `patch`;
    /// `null` when the byte budget ran out before this file.
    pub patch: Option<String>,
    /// True if `patch` was cut at the byte budget and won't apply as-is; raise `max_bytes` for the rest.
    pub patch_truncated: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiffTotals {
    /// Number of changed files, across all pages.
    pub files: usize,
    /// Lines added across all pages; `null` with `name_status`.
    pub additions: Option<u64>,
    /// Lines deleted across all pages; `null` with `name_status`.
    pub deletions: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiffResult {
    /// Which two states were compared.
    pub mode: DiffMode,
    /// Full commit SHA of the old side; `null` when it is the index or not a commit (a tree SHA, or the
    /// empty tree of a repository without commits).
    pub from: Option<String>,
    /// Full commit SHA of the new side; `null` when it is the index or the working tree.
    pub to: Option<String>,
    /// Changed files in path order.
    pub files: Vec<DiffFile>,
    /// Sums over every changed file, not just this page.
    pub totals: DiffTotals,
    /// Paths with unresolved merge conflicts. They aren't diffed; use status for their conflict kind.
    pub unmerged: Vec<String>,
    /// True if `files` was cut off at `limit` or a patch was dropped or cut for the byte budget.
    pub truncated: bool,
}

/// One side of a changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Side {
    /// Object id, or null for content that lives only in the working tree.
    id: ObjectId,
    kind: EntryKind,
    path: BString,
}

/// A changed path with both of its sides, as found by one of the comparisons.
#[derive(Debug)]
pub struct FileChange {
    pub status: ChangeKind,
    old: Option<Side>,
    new: Option<Side>,
}

impl FileChange {
    /// The new path, or the old one for deletions.
    pub fn path(&self) -> &BStr {
        self.new
            .as_ref()
            .or(self.old.as_ref())
            .map_or_else(Default::default, |s| s.path.as_ref())
    }

    /// The source path of a rename or copy.
    pub fn old_path(&self) -> Option<&BStr> {
        matches!(self.status, ChangeKind::Renamed | ChangeKind::Copied)
            .then(|| self.old.as_ref().map(|s| s.path.as_ref()))
            .flatten()
    }
}

/// The changes to report, plus what was compared.
struct Comparison {
    mode: DiffMode,
    from: Option<ObjectId>,
    to: Option<ObjectId>,
    changes: Vec<FileChange>,
    unmerged: Vec<String>,
}

pub fn diff(repo: &gix::Repository, args: DiffArgs) -> anyhow::Result<DiffResult> {
    let mut comparison = match (&args.from, &args.to, args.staged) {
        (_, Some(_), true) => bail!("`to` can't be combined with `staged`; drop one of them"),
        (None, Some(_), false) => bail!("`to` needs `from`; pass both revisions to compare"),
        (Some(from), Some(to), false) => between_revisions(repo, from, to, &args.pathspec)?,
        (from, None, true) => staged(repo, from.as_deref(), &args.pathspec)?,
        (Some(_), None, false) => bail!(
            "comparing a revision with the working tree isn't supported; pass `to` for two revisions, \
             or set `staged: true` to compare `from` with the index"
        ),
        (None, None, false) => worktree(repo, &args.pathspec)?,
    };
    comparison.changes.sort_by(|a, b| a.path().cmp(b.path()));
    render(repo, comparison, &args)
}

fn between_revisions(
    repo: &gix::Repository,
    from: &str,
    to: &str,
    pathspec: &[String],
) -> anyhow::Result<Comparison> {
    let old = resolve_tree(repo, Some(from))?;
    let new = resolve_tree(repo, Some(to))?;
    let index = repo.index_or_empty()?;
    let mut spec = pathspec_search(repo, pathspec, &index)?;
    let mut changes: Vec<_> = repo
        .diff_tree_to_tree(&old, &new, None)?
        .into_iter()
        .filter_map(from_tree_change)
        .collect();
    repair_copies(&mut changes, |p| tree_side(&old, p), |p| tree_side(&new, p))?;
    changes.retain(|c| {
        spec.as_mut()
            .is_none_or(|s| s.is_included(c.path(), Some(false)))
    });
    Ok(Comparison {
        mode: DiffMode::Revisions,
        from: commit_id_of(repo, from),
        to: commit_id_of(repo, to),
        changes,
        unmerged: Vec::new(),
    })
}

fn staged(
    repo: &gix::Repository,
    from: Option<&str>,
    pathspec: &[String],
) -> anyhow::Result<Comparison> {
    require_worktree(repo)?;
    // Like `git diff --cached`, a repository without commits compares the index with the empty tree.
    let tree = match from {
        Some(rev) => resolve_tree(repo, Some(rev))?.id,
        None => repo.head_tree_id_or_empty()?.detach(),
    };
    let (changes, unmerged) = index_changes(repo, &tree, pathspec)?;
    Ok(Comparison {
        mode: DiffMode::Staged,
        from: commit_id_of(repo, from.unwrap_or("HEAD")),
        to: None,
        changes,
        unmerged,
    })
}

/// Changes from `tree` to the index, with rename (and configured copy) detection, plus the paths with
/// unresolved conflicts, which are left out of the changes.
pub fn index_changes(
    repo: &gix::Repository,
    tree: &gix::oid,
    pathspec: &[String],
) -> anyhow::Result<(Vec<FileChange>, Vec<String>)> {
    let index = repo.index_or_empty()?;
    let unmerged = unmerged_paths(&index);
    let mut spec = pathspec_search(repo, pathspec, &index)?;
    let mut changes = Vec::new();
    repo.tree_index_status(
        tree,
        &index,
        spec.as_mut(),
        index_rename_tracking(repo),
        |change, _, _| {
            // Unmerged entries can't be compared, so their paths would otherwise show up as deleted.
            let change = from_index_change(change.into_owned());
            if !unmerged.iter().any(|p| change.path() == p.as_str()) {
                changes.push(change);
            }
            Ok(std::ops::ControlFlow::Continue(()))
        },
    )?;
    let tree = repo.find_tree(tree.to_owned())?;
    repair_copies(
        &mut changes,
        |p| tree_side(&tree, p),
        |p| Ok(index_side(&index, p)),
    )?;
    Ok((changes, unmerged))
}

/// Rename tracking between a tree and the index, configured like git: `status.renames`, falling back
/// to `diff.renames`, on by default; `copies` also tracks copies. gix 0.90 looks the first key up as
/// `merge.renames`, so resolve it here.
fn index_rename_tracking(repo: &gix::Repository) -> TrackRenames {
    let config = repo.config_snapshot();
    let Some((key, value)) = ["status.renames", "diff.renames"]
        .into_iter()
        .find_map(|key| config.string(key).map(|value| (key, value)))
    else {
        return TrackRenames::Given(Rewrites::default());
    };
    if value.eq_ignore_ascii_case(b"copies") || value.eq_ignore_ascii_case(b"copy") {
        return TrackRenames::Given(Rewrites {
            copies: Some(Default::default()),
            ..Default::default()
        });
    }
    match config.boolean(key) {
        Some(false) => TrackRenames::Disabled,
        _ => TrackRenames::Given(Rewrites::default()),
    }
}

/// gix 0.90 reports a copy whose source was itself modified as a copy of the source's *new* content,
/// and drops the source's own modification. Git copies from the old content and still lists the
/// source, so point each copy back at the old content and restore the source's change.
fn repair_copies(
    changes: &mut Vec<FileChange>,
    old_side: impl Fn(&BStr) -> anyhow::Result<Option<Side>>,
    new_side: impl Fn(&BStr) -> anyhow::Result<Option<Side>>,
) -> anyhow::Result<()> {
    let mut restored: Vec<FileChange> = Vec::new();
    for copy in changes
        .iter_mut()
        .filter(|c| c.status == ChangeKind::Copied)
    {
        let Some(source) = copy.old.as_ref().map(|s| s.path.clone()) else {
            continue;
        };
        let (before, after) = (old_side(source.as_ref())?, new_side(source.as_ref())?);
        if before.is_some() {
            copy.old.clone_from(&before);
        }
        if before == after || restored.iter().any(|r| r.path() == source) {
            continue;
        }
        let status = match (&before, &after) {
            (Some(old), Some(new)) => ChangeKind::of_modification(old.kind, new.kind),
            (None, _) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Deleted,
        };
        restored.push(FileChange {
            status,
            old: before,
            new: after,
        });
    }
    restored.retain(|r| !changes.iter().any(|c| c.path() == r.path()));
    changes.extend(restored);
    Ok(())
}

/// The non-directory entry at `path` in `tree`.
fn tree_side(tree: &gix::Tree<'_>, path: &BStr) -> anyhow::Result<Option<Side>> {
    let entry = tree.lookup_entry(path.split(|b| *b == b'/'))?;
    Ok(entry.filter(|e| !e.mode().is_tree()).map(|e| Side {
        id: e.object_id(),
        kind: e.mode().kind(),
        path: path.to_owned(),
    }))
}

/// The unconflicted index entry at `path`.
fn index_side(index: &gix::index::State, path: &BStr) -> Option<Side> {
    index
        .entry_by_path_and_stage(path, gix::index::entry::Stage::Unconflicted)
        .map(|e| Side {
            id: e.id,
            kind: index_entry_kind(e.mode),
            path: path.to_owned(),
        })
}

fn worktree(repo: &gix::Repository, pathspec: &[String]) -> anyhow::Result<Comparison> {
    require_worktree(repo)?;
    let iter = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::None)
        .into_index_worktree_iter(pathspec_patterns(pathspec))?;
    let mut changes = Vec::new();
    let mut unmerged = Vec::new();
    for item in iter {
        let index_worktree::Item::Modification {
            entry,
            rela_path,
            status,
            ..
        } = item?
        else {
            continue;
        };
        let old = Side {
            id: entry.id,
            kind: index_entry_kind(entry.mode),
            path: rela_path.clone(),
        };
        let on_disk = |kind| Side {
            id: ObjectId::null(entry.id.kind()),
            kind,
            path: rela_path.clone(),
        };
        let change = match status {
            EntryStatus::Conflict { .. } => {
                unmerged.push(rela_path.to_string());
                continue;
            }
            EntryStatus::NeedsUpdate(_) => continue,
            EntryStatus::IntentToAdd => FileChange {
                status: ChangeKind::Added,
                old: None,
                new: Some(on_disk(old.kind)),
            },
            EntryStatus::Change(Change::Removed) => FileChange {
                status: ChangeKind::Deleted,
                old: Some(old),
                new: None,
            },
            EntryStatus::Change(Change::Type { worktree_mode }) => FileChange {
                status: ChangeKind::Typechange,
                new: Some(on_disk(index_entry_kind(worktree_mode))),
                old: Some(old),
            },
            EntryStatus::Change(Change::Modification {
                executable_bit_changed,
                ..
            }) => FileChange {
                status: ChangeKind::Modified,
                new: Some(on_disk(if executable_bit_changed {
                    toggle_executable(old.kind)
                } else {
                    old.kind
                })),
                old: Some(old),
            },
            // Like `git diff --ignore-submodules=dirty`: only a different checked-out commit is a
            // change; edits inside the submodule belong to its own repository.
            EntryStatus::Change(Change::SubmoduleModification(sub)) => {
                let Some(head) = sub.checked_out_head_id.filter(|head| *head != entry.id) else {
                    continue;
                };
                FileChange {
                    status: ChangeKind::Modified,
                    new: Some(Side {
                        id: head,
                        kind: EntryKind::Commit,
                        path: rela_path.clone(),
                    }),
                    old: Some(old),
                }
            }
        };
        changes.push(change);
    }
    unmerged.sort();
    unmerged.dedup();
    Ok(Comparison {
        mode: DiffMode::Worktree,
        from: None,
        to: None,
        changes,
        unmerged,
    })
}

fn require_worktree(repo: &gix::Repository) -> anyhow::Result<()> {
    if repo.workdir().is_none() {
        bail!(
            "this repository is bare, so there is no index or worktree to diff; pass `from` and `to` revisions"
        );
    }
    Ok(())
}

fn unmerged_paths(index: &gix::index::State) -> Vec<String> {
    let mut paths: Vec<_> = index
        .entries()
        .iter()
        .filter(|e| e.stage() != gix::index::entry::Stage::Unconflicted)
        .map(|e| e.path(index).to_string())
        .collect();
    paths.dedup();
    paths
}

/// The commit a revision names, if it names one (a bare tree SHA doesn't).
fn commit_id_of(repo: &gix::Repository, rev: &str) -> Option<ObjectId> {
    resolve_commit(repo, Some(rev)).ok().map(|c| c.id)
}

fn toggle_executable(kind: EntryKind) -> EntryKind {
    match kind {
        EntryKind::Blob => EntryKind::BlobExecutable,
        EntryKind::BlobExecutable => EntryKind::Blob,
        other => other,
    }
}

/// Convert a tree-diff change, dropping directories: their files are reported individually.
fn from_tree_change(change: gix::object::tree::diff::ChangeDetached) -> Option<FileChange> {
    use gix::object::tree::diff::ChangeDetached as C;
    let side = |id, mode: gix::object::tree::EntryMode, path| {
        (!mode.is_tree()).then_some(Side {
            id,
            kind: mode.kind(),
            path,
        })
    };
    let (status, old, new) = match change {
        C::Addition {
            location,
            entry_mode,
            id,
            ..
        } => (ChangeKind::Added, None, side(id, entry_mode, location)),
        C::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => (ChangeKind::Deleted, side(id, entry_mode, location), None),
        C::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => {
            let status = ChangeKind::of_modification(previous_entry_mode.kind(), entry_mode.kind());
            (
                status,
                side(previous_id, previous_entry_mode, location.clone()),
                side(id, entry_mode, location),
            )
        }
        C::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            entry_mode,
            id,
            location,
            copy,
            ..
        } => (
            if copy {
                ChangeKind::Copied
            } else {
                ChangeKind::Renamed
            },
            side(source_id, source_entry_mode, source_location),
            side(id, entry_mode, location),
        ),
    };
    // A directory replaced by a file (or the reverse) leaves only the file side.
    let status = match (&old, &new) {
        (None, None) => return None,
        (None, Some(_)) => ChangeKind::Added,
        (Some(_), None) => ChangeKind::Deleted,
        (Some(_), Some(_)) => status,
    };
    Some(FileChange { status, old, new })
}

fn from_index_change(change: gix::diff::index::Change) -> FileChange {
    use gix::diff::index::ChangeRef as C;
    let side = |id: &gix::oid, mode, path: &BStr| Side {
        id: id.to_owned(),
        kind: index_entry_kind(mode),
        path: path.to_owned(),
    };
    match change {
        C::Addition {
            location,
            entry_mode,
            id,
            ..
        } => FileChange {
            status: ChangeKind::Added,
            old: None,
            new: Some(side(&id, entry_mode, location.as_ref())),
        },
        C::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => FileChange {
            status: ChangeKind::Deleted,
            old: Some(side(&id, entry_mode, location.as_ref())),
            new: None,
        },
        C::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
            ..
        } => FileChange {
            status: ChangeKind::of_modification(
                index_entry_kind(previous_entry_mode),
                index_entry_kind(entry_mode),
            ),
            old: Some(side(&previous_id, previous_entry_mode, location.as_ref())),
            new: Some(side(&id, entry_mode, location.as_ref())),
        },
        C::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            location,
            entry_mode,
            id,
            copy,
            ..
        } => FileChange {
            status: if copy {
                ChangeKind::Copied
            } else {
                ChangeKind::Renamed
            },
            old: Some(side(
                &source_id,
                source_entry_mode,
                source_location.as_ref(),
            )),
            new: Some(side(&id, entry_mode, location.as_ref())),
        },
    }
}

/// Load contents, count lines and render patches for every change; only the requested page is returned.
fn render(
    repo: &gix::Repository,
    comparison: Comparison,
    args: &DiffArgs,
) -> anyhow::Result<DiffResult> {
    let total = comparison.changes.len();
    let skip = args.skip.unwrap_or(0).min(total);
    let limit = page_limit(args.limit, DEFAULT_DIFF_LIMIT, MAX_DIFF_LIMIT);
    let page = skip..total.min(skip.saturating_add(limit));
    let mut truncated = page.end < total;

    let mut loader = (args.format != DiffFormat::NameStatus)
        .then(|| ContentLoader::new(repo, comparison.mode))
        .transpose()?;
    let opts = PatchOptions {
        hash: repo.object_hash(),
        algorithm: repo.diff_algorithm()?,
        context: args.context_lines.unwrap_or(DEFAULT_CONTEXT_LINES),
    };
    let mut budget = PatchBudget::new(byte_budget(args.max_bytes));
    let (mut additions, mut deletions) = (0u64, 0u64);
    let mut files = Vec::with_capacity(page.len());
    for (i, change) in comparison.changes.iter().enumerate() {
        let in_page = page.contains(&i);
        let Some(loader) = loader.as_mut() else {
            if in_page {
                files.push(name_status_entry(change));
            }
            continue;
        };
        let (old, new) = (
            loader.load(change.old.as_ref())?,
            loader.load(change.new.as_ref())?,
        );
        let line_diff = LineDiff::compute(opts.algorithm, &old, &new);
        let stats = line_diff.as_ref().map(LineDiff::stats);
        if let Some((added, deleted)) = stats {
            additions += u64::from(added);
            deletions += u64::from(deleted);
        }
        if !in_page {
            continue;
        }
        let mut file = name_status_entry(change);
        file.binary = stats.is_none();
        (file.additions, file.deletions) = stats.map_or((None, None), |(a, d)| (Some(a), Some(d)));
        if args.format == DiffFormat::Patch {
            let text = patch_text(change, &old, &new, line_diff.as_ref(), opts)?;
            (file.patch, file.patch_truncated) = budget.admit(text);
            truncated |= file.patch.is_none() || file.patch_truncated;
        }
        files.push(file);
    }

    let counted = loader.is_some();
    Ok(DiffResult {
        mode: comparison.mode,
        from: comparison.from.map(|id| id.to_string()),
        to: comparison.to.map(|id| id.to_string()),
        files,
        totals: DiffTotals {
            files: total,
            additions: counted.then_some(additions),
            deletions: counted.then_some(deletions),
        },
        unmerged: comparison.unmerged,
        truncated,
    })
}

fn name_status_entry(change: &FileChange) -> DiffFile {
    DiffFile {
        path: change.path().to_string(),
        old_path: change.old_path().map(|p| p.to_string()),
        status: change.status,
        additions: None,
        deletions: None,
        binary: false,
        patch: None,
        patch_truncated: false,
    }
}

/// Hands out patch text until the byte budget is spent. The first patch of a response is cut to fit
/// rather than dropped, so even a single huge file shows something.
struct PatchBudget {
    remaining: usize,
    exhausted: bool,
    first: bool,
}

impl PatchBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            remaining: max_bytes,
            exhausted: false,
            first: true,
        }
    }

    /// Returns the patch to report and whether it was cut.
    fn admit(&mut self, text: String) -> (Option<String>, bool) {
        let first = std::mem::replace(&mut self.first, false);
        if self.exhausted {
            return (None, false);
        }
        if text.len() <= self.remaining {
            self.remaining -= text.len();
            return (Some(text), false);
        }
        self.exhausted = true;
        if !first {
            return (None, false);
        }
        let mut end = self.remaining;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let cut = text[..end].rfind('\n').map_or(0, |i| i + 1);
        (Some(text[..cut].to_owned()), true)
    }
}

/// File content as git would diff it.
enum Content {
    Text { data: Vec<u8>, id: ObjectId },
    Binary { id: ObjectId },
    Missing,
}

impl Content {
    fn id(&self, hash: gix::hash::Kind) -> ObjectId {
        match self {
            Content::Text { id, .. } | Content::Binary { id } => *id,
            Content::Missing => ObjectId::null(hash),
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Content::Text { data, .. } => data,
            Content::Binary { .. } | Content::Missing => &[],
        }
    }
}

/// Reads both sides of changes through gix's diff pipeline, which applies git's filters
/// (e.g. CRLF conversion) to worktree files and detects binary content like git does.
struct ContentLoader<'repo> {
    repo: &'repo gix::Repository,
    cache: gix::diff::blob::Platform,
}

impl<'repo> ContentLoader<'repo> {
    fn new(repo: &'repo gix::Repository, mode: DiffMode) -> anyhow::Result<Self> {
        let roots = WorktreeRoots {
            old_root: None,
            new_root: (mode == DiffMode::Worktree)
                .then(|| repo.workdir().map(ToOwned::to_owned))
                .flatten(),
        };
        Ok(Self {
            repo,
            cache: repo.diff_resource_cache(Mode::ToGit, roots)?,
        })
    }

    fn load(&mut self, side: Option<&Side>) -> anyhow::Result<Content> {
        let Some(side) = side else {
            return Ok(Content::Missing);
        };
        if side.kind == EntryKind::Commit {
            // A submodule diffs as a one-line pseudo-file naming the commit, exactly as git shows it.
            return Ok(Content::Text {
                data: format!("Subproject commit {}\n", side.id).into_bytes(),
                id: side.id,
            });
        }
        // The worktree side goes into the "new" slot, which is the only one with a worktree root.
        let slot = if side.id.is_null() {
            ResourceKind::NewOrDestination
        } else {
            ResourceKind::OldOrSource
        };
        self.cache
            .set_resource(
                side.id,
                side.kind,
                side.path.as_ref(),
                slot,
                &self.repo.objects,
            )
            .map_err(gix::Error::from)?;
        let resource = self
            .cache
            .resource(slot)
            .ok_or_else(|| anyhow::anyhow!("diff resource vanished"))?;
        let content = match resource.data {
            Data::Missing => Content::Missing,
            Data::Binary { .. } => Content::Binary {
                id: worktree_aware_id(self.repo, side.id, None)?,
            },
            Data::Buffer { buf, .. } => Content::Text {
                id: worktree_aware_id(self.repo, side.id, Some(buf))?,
                data: buf.to_vec(),
            },
        };
        self.cache.clear_resource_cache_keep_allocation();
        Ok(content)
    }
}

/// Objects already have ids; worktree content gets the id it would have once staged.
fn worktree_aware_id(
    repo: &gix::Repository,
    id: ObjectId,
    data: Option<&[u8]>,
) -> anyhow::Result<ObjectId> {
    match data {
        Some(data) if id.is_null() => Ok(gix::objs::compute_hash(
            repo.object_hash(),
            gix::object::Kind::Blob,
            data,
        )?),
        _ => Ok(id),
    }
}

/// Settings shared by every patch in a response.
#[derive(Clone, Copy)]
struct PatchOptions {
    hash: gix::hash::Kind,
    algorithm: Algorithm,
    context: u32,
}

/// A line diff of two text contents.
struct LineDiff<'a> {
    input: InternedInput<&'a [u8]>,
    diff: gix::diff::blob::Diff,
}

impl<'a> LineDiff<'a> {
    /// `None` if either side is binary.
    fn compute(algorithm: Algorithm, old: &'a Content, new: &'a Content) -> Option<Self> {
        if matches!(old, Content::Binary { .. }) || matches!(new, Content::Binary { .. }) {
            return None;
        }
        // Lines keep their terminators, as in git: a last line without a newline differs from the
        // same line with one, which is what lets the patch say "No newline at end of file".
        let input = InternedInput::new(
            gix::diff::blob::sources::byte_lines(old.bytes()),
            gix::diff::blob::sources::byte_lines(new.bytes()),
        );
        let diff = gix::diff::blob::diff_with_slider_heuristics(algorithm, &input);
        Some(Self { input, diff })
    }

    /// `(added, deleted)` lines.
    fn stats(&self) -> (u32, u32) {
        (self.diff.count_additions(), self.diff.count_removals())
    }

    /// Unified-diff hunks; `old` is the old side's content, for function context lines.
    fn hunks(&self, old: &[u8], context: u32) -> std::io::Result<String> {
        UnifiedDiff::new(
            &self.diff,
            &self.input,
            HunkWriter::new(old),
            ContextSize::symmetrical(context),
        )
        .consume()
    }
}

/// Git-style patch text for one file, starting at `diff --git`. `line_diff` is `None` for binary files.
fn patch_text(
    change: &FileChange,
    old: &Content,
    new: &Content,
    line_diff: Option<&LineDiff<'_>>,
    opts: PatchOptions,
) -> anyhow::Result<String> {
    // Git shows a type change (file <-> symlink <-> submodule) as a deletion followed by an addition.
    if change.status == ChangeKind::Typechange {
        let deletion = FileChange {
            status: ChangeKind::Deleted,
            old: change.old.clone(),
            new: None,
        };
        let addition = FileChange {
            status: ChangeKind::Added,
            old: None,
            new: change.new.clone(),
        };
        let none = Content::Missing;
        let removed = LineDiff::compute(opts.algorithm, old, &none);
        let added = LineDiff::compute(opts.algorithm, &none, new);
        let mut text = patch_text(&deletion, old, &none, removed.as_ref(), opts)?;
        text.push_str(&patch_text(&addition, &none, new, added.as_ref(), opts)?);
        return Ok(text);
    }

    let old_path = change
        .old
        .as_ref()
        .map_or_else(|| change.path(), |s| s.path.as_ref());
    let new_path = change.path();
    let old_mode = change.old.as_ref().map(|s| s.kind);
    let new_mode = change.new.as_ref().map(|s| s.kind);

    let mut out = String::new();
    writeln!(
        out,
        "diff --git {} {}",
        quote_path("a/", old_path),
        quote_path("b/", new_path)
    )?;
    match (old_mode, new_mode) {
        (None, Some(mode)) => writeln!(out, "new file mode {}", octal_mode(mode))?,
        (Some(mode), None) => writeln!(out, "deleted file mode {}", octal_mode(mode))?,
        (Some(a), Some(b)) if a != b => writeln!(
            out,
            "old mode {}\nnew mode {}",
            octal_mode(a),
            octal_mode(b)
        )?,
        _ => {}
    }
    if let Some(source) = change.old_path() {
        let verb = if change.status == ChangeKind::Copied {
            "copy"
        } else {
            "rename"
        };
        writeln!(
            out,
            "{verb} from {}\n{verb} to {}",
            quote_path("", source),
            quote_path("", new_path)
        )?;
    }

    let (old_id, new_id) = (old.id(opts.hash), new.id(opts.hash));
    if old_id != new_id {
        write!(out, "index {}..{}", short_hex(&old_id), short_hex(&new_id))?;
        match (old_mode, new_mode) {
            (Some(a), Some(b)) if a == b => writeln!(out, " {}", octal_mode(a))?,
            _ => out.push('\n'),
        }
    }

    let old_label = old_mode.map_or_else(|| "/dev/null".to_owned(), |_| quote_path("a/", old_path));
    let new_label = new_mode.map_or_else(|| "/dev/null".to_owned(), |_| quote_path("b/", new_path));
    let Some(line_diff) = line_diff else {
        writeln!(out, "Binary files {old_label} and {new_label} differ")?;
        return Ok(out);
    };
    let hunks = line_diff.hunks(old.bytes(), opts.context)?;
    if !hunks.is_empty() {
        writeln!(out, "--- {old_label}\n+++ {new_label}")?;
        out.push_str(&hunks);
    }
    Ok(out)
}

/// Git's default funcname rule cuts the hunk-header context line to this many bytes.
const FUNC_CONTEXT_MAX: usize = 80;

/// Collects unified-diff hunks in git's exact format: compact `@@` ranges, the function-context
/// line after the header, and "No newline at end of file" markers.
struct HunkWriter<'a> {
    text: String,
    /// Lines of the old side, with terminators, to find each hunk's function context.
    old_lines: Vec<&'a [u8]>,
}

impl<'a> HunkWriter<'a> {
    fn new(old: &'a [u8]) -> Self {
        Self {
            text: String::new(),
            old_lines: old.split_inclusive(|b| *b == b'\n').collect(),
        }
    }

    /// Git's default (no diff driver) funcname: the nearest line above the hunk that starts with an
    /// ASCII letter, `_` or `$`, cut to 80 bytes and stripped of trailing whitespace.
    fn function_context(&self, hunk_start: u32) -> Option<String> {
        let above = (hunk_start as usize)
            .saturating_sub(1)
            .min(self.old_lines.len());
        let line = self.old_lines[..above].iter().rev().find(|l| {
            l.first()
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_' || *b == b'$')
        })?;
        let line = &line[..line.len().min(FUNC_CONTEXT_MAX)];
        Some(
            line.trim_end_with(|c| c.is_ascii_whitespace())
                .to_str_lossy()
                .into_owned(),
        )
    }
}

/// `start,len` as git prints it: `,1` is omitted, and an empty range names the line before it.
fn hunk_range(start: u32, len: u32) -> String {
    match len {
        0 => format!("{},0", start.saturating_sub(1)),
        1 => start.to_string(),
        _ => format!("{start},{len}"),
    }
}

impl ConsumeHunk for HunkWriter<'_> {
    type Out = String;

    fn consume_hunk(
        &mut self,
        header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        let old = hunk_range(header.before_hunk_start, header.before_hunk_len);
        let new = hunk_range(header.after_hunk_start, header.after_hunk_len);
        let context = self.function_context(header.before_hunk_start);
        let line = match context {
            Some(func) => format!("@@ -{old} +{new} @@ {func}\n"),
            None => format!("@@ -{old} +{new} @@\n"),
        };
        self.text.push_str(&line);
        for (kind, line) in lines {
            self.text.push(kind.to_prefix());
            self.text.push_str(&line.to_str_lossy());
            if !line.ends_with(b"\n") {
                self.text.push_str("\n\\ No newline at end of file\n");
            }
        }
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self.text
    }
}

/// `prefix + path`, C-quoted like git when the path has characters that would break patch parsing.
fn quote_path(prefix: &str, path: &BStr) -> String {
    let path = path.to_str_lossy();
    if !path.contains(['"', '\\', '\t', '\n']) {
        return format!("{prefix}{path}");
    }
    let escaped = path
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\t', "\\t")
        .replace('\n', "\\n");
    format!("\"{prefix}{escaped}\"")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use super::*;
    use crate::tools::testutil::{
        bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git, try_git, write,
    };

    fn args() -> DiffArgs {
        DiffArgs {
            from: None,
            to: None,
            staged: false,
            pathspec: vec![],
            format: DiffFormat::Stat,
            context_lines: None,
            max_bytes: None,
            limit: None,
            skip: None,
        }
    }

    fn revs(from: &str, to: &str) -> DiffArgs {
        DiffArgs {
            from: Some(from.into()),
            to: Some(to.into()),
            ..args()
        }
    }

    fn with_format(mut a: DiffArgs, format: DiffFormat) -> DiffArgs {
        a.format = format;
        a
    }

    /// `git diff --numstat` rows as `(added, deleted, path)`; binary files show `-`.
    fn git_numstat(dir: &Path, extra: &[&str]) -> BTreeSet<(String, String, String)> {
        let mut cmd = vec![
            "-c",
            "core.quotePath=false",
            "diff",
            "--numstat",
            "--no-renames",
        ];
        cmd.extend_from_slice(extra);
        git_stdout(dir, &cmd)
            .lines()
            .map(|l| {
                let f: Vec<_> = l.splitn(3, '\t').collect();
                (f[0].to_string(), f[1].to_string(), f[2].to_string())
            })
            .collect()
    }

    fn our_numstat(res: &DiffResult) -> BTreeSet<(String, String, String)> {
        let n = |v: Option<u32>| v.map_or("-".to_string(), |v| v.to_string());
        res.files
            .iter()
            .map(|f| (n(f.additions), n(f.deletions), f.path.clone()))
            .collect()
    }

    fn git_name_status(dir: &Path, extra: &[&str]) -> BTreeSet<String> {
        let mut cmd = vec!["-c", "core.quotePath=false", "diff", "--name-status"];
        cmd.extend_from_slice(extra);
        git_stdout(dir, &cmd)
            .lines()
            .map(|l| {
                // Drop rename similarity scores like `R100`, which we don't report.
                let (status, rest) = l.split_once('\t').unwrap();
                format!("{}\t{rest}", &status[..1])
            })
            .collect()
    }

    fn our_name_status(res: &DiffResult) -> BTreeSet<String> {
        res.files
            .iter()
            .map(|f| {
                let code = match f.status {
                    ChangeKind::Added => "A",
                    ChangeKind::Modified => "M",
                    ChangeKind::Deleted => "D",
                    ChangeKind::Renamed => "R",
                    ChangeKind::Copied => "C",
                    ChangeKind::Typechange => "T",
                };
                match &f.old_path {
                    Some(old) => format!("{code}\t{old}\t{}", f.path),
                    None => format!("{code}\t{}", f.path),
                }
            })
            .collect()
    }

    fn full_patch(res: &DiffResult) -> String {
        res.files.iter().filter_map(|f| f.patch.clone()).collect()
    }

    /// Apply our patch to `base` in a scratch clone and check the result matches `target`,
    /// apart from `skip` paths (binary files, whose one-line patches can't be applied).
    fn assert_patch_reproduces(dir: &Path, patch: &str, base: &str, target: &str, skip: &[&str]) {
        let scratch = tempfile::tempdir().unwrap();
        let patch_file = scratch.path().join("ours.patch");
        std::fs::write(&patch_file, patch).unwrap();
        let clone = scratch.path().join("clone");
        run_git(
            scratch.path(),
            &[
                "clone",
                "-q",
                dir.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run_git(&clone, &["checkout", "-q", base]);
        assert!(
            try_git(&clone, &["apply", "--index", patch_file.to_str().unwrap()]),
            "patch failed to apply:\n{patch}"
        );
        let target = git_stdout(dir, &["rev-parse", target]);
        let mut cmd = vec!["diff", "--cached", "--name-only", target.trim(), "--", "."];
        let excludes: Vec<_> = skip.iter().map(|p| format!(":(exclude){p}")).collect();
        cmd.extend(excludes.iter().map(String::as_str));
        assert_eq!(
            git_stdout(&clone, &cmd),
            "",
            "applied patch differs from {target}"
        );
    }

    /// A commit touching every kind of change: edits, additions, deletions, a rename, a mode change,
    /// a typechange, a binary file, a missing final newline, and a unicode path.
    fn change_fixture() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let body: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        write(root, "long.txt", &body);
        write(root, "rename-me.txt", "keep\nthese\nlines\nstable\n");
        write(root, "delete-me.txt", "bye\n");
        write(root, "script.sh", "echo hi\n");
        write(root, "becomes-link.txt", "target\n");
        write(root, "no-eol.txt", "no newline");
        write(root, "data.bin", [0u8, 1, 2, 3]);
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "base"]);
        run_git(root, &["tag", "base"]);

        write(
            root,
            "long.txt",
            body.replace("line 3\n", "line three\n")
                .replace("line 27\n", ""),
        );
        run_git(root, &["mv", "rename-me.txt", "renamed.txt"]);
        std::fs::remove_file(root.join("delete-me.txt")).unwrap();
        std::fs::set_permissions(
            root.join("script.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::fs::remove_file(root.join("becomes-link.txt")).unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("becomes-link.txt")).unwrap();
        write(root, "no-eol.txt", "no newline\nnow two lines");
        write(root, "data.bin", [9u8, 0, 9]);
        write(root, "ünïcode dir/new file.txt", "fresh\n");
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-q", "-m", "changes"]);
        let repo = open(root);
        (dir, repo)
    }

    #[test]
    fn revisions_match_git_numstat_and_name_status() {
        let (dir, repo) = change_fixture();
        let root = dir.path();
        let stat = diff(&repo, revs("base", "HEAD")).unwrap();
        assert_eq!(stat.mode, DiffMode::Revisions);
        assert_eq!(
            our_name_status(&stat),
            git_name_status(root, &["base", "HEAD"])
        );

        // numstat with rename detection off splits the rename into delete+add; compare the rest.
        let ours: BTreeSet<_> = our_numstat(&stat)
            .into_iter()
            .filter(|(_, _, p)| p != "renamed.txt" && p != "becomes-link.txt")
            .collect();
        let git: BTreeSet<_> = git_numstat(root, &["base", "HEAD"])
            .into_iter()
            .filter(|(_, _, p)| {
                !["renamed.txt", "rename-me.txt", "becomes-link.txt"].contains(&p.as_str())
            })
            .collect();
        assert_eq!(ours, git);

        let totals = &stat.totals;
        assert_eq!(totals.files, stat.files.len());
        let shortstat = git_stdout(root, &["diff", "--shortstat", "base", "HEAD"]);
        assert!(
            shortstat.contains(&format!("{} insertion", totals.additions.unwrap())),
            "{shortstat}"
        );
        assert!(
            shortstat.contains(&format!("{} deletion", totals.deletions.unwrap())),
            "{shortstat}"
        );
        assert_eq!(
            stat.from,
            Some(git_stdout(root, &["rev-parse", "base"]).trim().to_string())
        );
        assert!(!stat.truncated);
    }

    #[test]
    fn revision_patch_applies_cleanly() {
        let (dir, repo) = change_fixture();
        let res = diff(&repo, with_format(revs("base", "HEAD"), DiffFormat::Patch)).unwrap();
        let patch: String = res
            .files
            .iter()
            .filter(|f| !f.binary)
            .filter_map(|f| f.patch.clone())
            .collect();
        assert!(patch.contains("\\ No newline at end of file"), "{patch}");
        assert!(
            patch.contains("rename from rename-me.txt\nrename to renamed.txt"),
            "{patch}"
        );
        assert!(
            patch.contains("old mode 100644\nnew mode 100755"),
            "{patch}"
        );
        assert!(patch.contains("new file mode 120000"), "{patch}");
        assert_patch_reproduces(dir.path(), &patch, "base", "HEAD", &["data.bin"]);
        let binary = res.files.iter().find(|f| f.path == "data.bin").unwrap();
        assert!(binary.binary && binary.additions.is_none());
        assert!(
            binary
                .patch
                .as_deref()
                .unwrap()
                .contains("Binary files a/data.bin and b/data.bin differ")
        );
    }

    #[test]
    fn patch_hunks_match_git_text() {
        let (dir, repo) = change_fixture();
        let mut a = with_format(revs("base", "HEAD"), DiffFormat::Patch);
        a.pathspec = vec!["long.txt".into()];
        let res = diff(&repo, a).unwrap();
        let ours = full_patch(&res);
        let git = git_stdout(dir.path(), &["diff", "base", "HEAD", "--", "long.txt"]);
        assert_eq!(ours, git);

        let mut a = with_format(revs("base", "HEAD"), DiffFormat::Patch);
        a.pathspec = vec!["long.txt".into()];
        a.context_lines = Some(0);
        let res = diff(&repo, a).unwrap();
        assert_eq!(
            full_patch(&res),
            git_stdout(
                dir.path(),
                &["diff", "-U0", "base", "HEAD", "--", "long.txt"]
            )
        );
    }

    #[test]
    fn worktree_and_staged_modes_match_git() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "b.txt", "bee\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "b"]);
        write(root, "a.txt", "one\ntwo\nthree\n");
        run_git(root, &["add", "a.txt"]);
        write(root, "a.txt", "one\n2\nthree\nfour\n");
        write(root, "b.txt", "bee\nbuzz\n");
        write(root, "staged-new.txt", "s\n");
        run_git(root, &["add", "staged-new.txt"]);
        write(root, "untracked.txt", "ignored by git diff\n");
        let repo = open(root);

        let unstaged = diff(&repo, args()).unwrap();
        assert_eq!(unstaged.mode, DiffMode::Worktree);
        assert_eq!(our_numstat(&unstaged), git_numstat(root, &[]));
        let patch = diff(&repo, with_format(args(), DiffFormat::Patch)).unwrap();
        assert_eq!(full_patch(&patch), git_stdout(root, &["diff"]));

        let mut a = args();
        a.staged = true;
        let staged = diff(&repo, a).unwrap();
        assert_eq!(staged.mode, DiffMode::Staged);
        assert_eq!(our_numstat(&staged), git_numstat(root, &["--cached"]));
        let mut a = with_format(args(), DiffFormat::Patch);
        a.staged = true;
        assert_eq!(
            full_patch(&diff(&repo, a).unwrap()),
            git_stdout(root, &["diff", "--cached"])
        );

        let mut a = args();
        a.staged = true;
        a.from = Some("HEAD~1".into());
        let older = diff(&repo, a).unwrap();
        assert_eq!(
            our_numstat(&older),
            git_numstat(root, &["--cached", "HEAD~1"])
        );
    }

    #[test]
    fn worktree_deletions_modes_and_symlinks() {
        let (dir, _) = change_fixture();
        let root = dir.path();
        std::fs::remove_file(root.join("long.txt")).unwrap();
        std::fs::set_permissions(
            root.join("script.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
        std::fs::remove_file(root.join("becomes-link.txt")).unwrap();
        write(root, "becomes-link.txt", "a file again\n");
        let repo = open(root);
        let res = diff(&repo, args()).unwrap();
        assert_eq!(our_name_status(&res), git_name_status(root, &[]));
        assert_eq!(our_numstat(&res), git_numstat(root, &[]));
        let patch = diff(&repo, with_format(args(), DiffFormat::Patch)).unwrap();
        assert_eq!(full_patch(&patch), git_stdout(root, &["diff"]));
    }

    #[test]
    fn worktree_intent_to_add_exec_bit_and_staged_deletion() {
        let (dir, _) = change_fixture();
        let root = dir.path();
        write(root, "planned.txt", "not staged yet\n");
        run_git(root, &["add", "-N", "planned.txt"]);
        std::fs::set_permissions(
            root.join("long.txt"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        run_git(root, &["rm", "-q", "a.txt"]);
        let repo = open(root);
        let patch = diff(&repo, with_format(args(), DiffFormat::Patch)).unwrap();
        assert_eq!(full_patch(&patch), git_stdout(root, &["diff"]));
        assert_eq!(our_numstat(&patch), git_numstat(root, &[]));

        let mut a = with_format(args(), DiffFormat::Patch);
        a.staged = true;
        let staged = diff(&repo, a).unwrap();
        assert_eq!(full_patch(&staged), git_stdout(root, &["diff", "--cached"]));
        assert_eq!(staged.files[0].status, ChangeKind::Deleted);
    }

    #[test]
    fn copies_follow_diff_renames_config() {
        let (dir, _) = change_fixture();
        let root = dir.path();
        run_git(root, &["config", "diff.renames", "copies"]);
        let body: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        write(root, "long-copy.txt", &body);
        write(root, "long.txt", format!("{body}line 31\n"));
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "copy long"]);
        let repo = open(root);
        let res = diff(
            &repo,
            with_format(revs("HEAD~1", "HEAD"), DiffFormat::Patch),
        )
        .unwrap();
        assert_eq!(
            our_name_status(&res),
            git_name_status(root, &["HEAD~1", "HEAD"])
        );
        let copy = res
            .files
            .iter()
            .find(|f| f.status == ChangeKind::Copied)
            .unwrap();
        assert_eq!(copy.old_path.as_deref(), Some("long.txt"));
        assert!(
            copy.patch
                .as_deref()
                .unwrap()
                .contains("copy from long.txt\ncopy to long-copy.txt")
        );
        assert_patch_reproduces(root, &full_patch(&res), "HEAD~1", "HEAD", &[]);

        run_git(root, &["reset", "-q", "--soft", "HEAD~1"]);
        let mut a = args();
        a.staged = true;
        let staged = diff(&open(root), a).unwrap();
        assert_eq!(
            our_name_status(&staged),
            git_name_status(root, &["--cached"])
        );
    }

    #[test]
    fn staged_in_a_repo_without_commits() {
        let (dir, repo) = empty_repo();
        write(dir.path(), "first.txt", "hello\n");
        run_git(dir.path(), &["add", "first.txt"]);
        let mut a = with_format(args(), DiffFormat::Patch);
        a.staged = true;
        let res = diff(&repo, a).unwrap();
        assert!(res.from.is_none());
        assert_eq!(
            full_patch(&res),
            git_stdout(dir.path(), &["diff", "--cached"])
        );
    }

    #[test]
    fn defaults_from_empty_arguments() {
        let parsed: DiffArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.format, DiffFormat::Stat);
        assert!(!parsed.staged && parsed.pathspec.is_empty());
        let parsed: DiffArgs = serde_json::from_str(r#"{"format":"name_status"}"#).unwrap();
        assert_eq!(parsed.format, DiffFormat::NameStatus);
    }

    #[test]
    fn staged_rename_is_detected() {
        let (dir, _) = change_fixture();
        let root = dir.path();
        run_git(root, &["mv", "long.txt", "longer.txt"]);
        let mut a = args();
        a.staged = true;
        let res = diff(&open(root), a).unwrap();
        assert_eq!(res.files.len(), 1);
        assert_eq!(res.files[0].status, ChangeKind::Renamed);
        assert_eq!(res.files[0].old_path.as_deref(), Some("long.txt"));
        assert_eq!(our_name_status(&res), git_name_status(root, &["--cached"]));
    }

    #[test]
    fn name_status_skips_content() {
        let (_dir, repo) = change_fixture();
        let res = diff(
            &repo,
            with_format(revs("base", "HEAD"), DiffFormat::NameStatus),
        )
        .unwrap();
        assert!(
            res.files
                .iter()
                .all(|f| f.additions.is_none() && f.patch.is_none())
        );
        assert!(res.totals.additions.is_none());
    }

    #[test]
    fn pathspec_limit_and_skip() {
        let (dir, repo) = change_fixture();
        let mut a = revs("base", "HEAD");
        a.pathspec = vec!["*.txt".into()];
        let res = diff(&repo, a).unwrap();
        let git = git_name_status(dir.path(), &["base", "HEAD", "--", "*.txt"]);
        assert_eq!(our_name_status(&res), git);

        let all = diff(&repo, revs("base", "HEAD")).unwrap();
        let mut a = revs("base", "HEAD");
        a.limit = Some(2);
        a.skip = Some(1);
        let page = diff(&repo, a).unwrap();
        assert_eq!(page.files.len(), 2);
        assert_eq!(page.files[0].path, all.files[1].path);
        assert!(page.truncated);
        assert_eq!(page.totals.files, all.totals.files);
        assert_eq!(page.totals.additions, all.totals.additions);
    }

    #[test]
    fn byte_budget_drops_later_patches_and_cuts_the_first() {
        let (_dir, repo) = change_fixture();
        let mut a = with_format(revs("base", "HEAD"), DiffFormat::Patch);
        a.max_bytes = Some(300);
        let res = diff(&repo, a).unwrap();
        assert!(res.truncated);
        let total: usize = res
            .files
            .iter()
            .filter_map(|f| f.patch.as_ref())
            .map(String::len)
            .sum();
        assert!(total <= 300, "{total}");
        assert!(res.files.iter().any(|f| f.patch.is_none()));

        let mut a = with_format(revs("base", "HEAD"), DiffFormat::Patch);
        a.pathspec = vec!["long.txt".into()];
        a.max_bytes = Some(60);
        let res = diff(&repo, a).unwrap();
        let file = &res.files[0];
        assert!(file.patch_truncated);
        assert!(
            file.patch
                .as_ref()
                .unwrap()
                .starts_with("diff --git a/long.txt b/long.txt\n")
        );
    }

    #[test]
    fn submodule_commits_diff_like_git() {
        let (sub, _) = fixture_repo();
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.path().to_str().unwrap(),
                "sub",
            ],
        );
        run_git(root, &["commit", "-q", "-m", "add submodule"]);
        write(root, "sub/a.txt", "moved on\n");
        run_git(&root.join("sub"), &["commit", "-q", "-am", "sub commit"]);
        let repo = open(root);
        let res = diff(&repo, with_format(args(), DiffFormat::Patch)).unwrap();
        assert_eq!(our_numstat(&res), git_numstat(root, &[]));
        assert_eq!(full_patch(&res), git_stdout(root, &["diff"]));
    }

    #[test]
    fn dirty_submodule_without_new_commit_is_not_a_change() {
        let (sub, _) = fixture_repo();
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.path().to_str().unwrap(),
                "sub",
            ],
        );
        run_git(root, &["commit", "-q", "-m", "add submodule"]);
        write(root, "sub/a.txt", "uncommitted edit\n");
        let res = diff(&open(root), args()).unwrap();
        assert!(res.files.is_empty());
        assert_eq!(git_stdout(root, &["diff", "--ignore-submodules=dirty"]), "");
    }

    #[test]
    fn patch_budget_cuts_on_char_and_line_boundaries() {
        let mut budget = PatchBudget::new(10); // byte 10 falls inside the fourth `ü`
        let (text, cut) = budget.admit("ab\nüüüü\nrest\n".to_owned());
        assert_eq!((text.as_deref(), cut), (Some("ab\n"), true));
        assert_eq!(
            budget.admit("x\n".to_owned()),
            (None, false),
            "budget is spent"
        );
        let mut budget = PatchBudget::new(4);
        assert_eq!(
            budget.admit("ok\n".to_owned()),
            (Some("ok\n".to_owned()), false)
        );
        assert_eq!(
            budget.admit("too long\n".to_owned()),
            (None, false),
            "only the first patch is cut"
        );
    }

    #[test]
    fn conflicts_are_listed_not_diffed() {
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
        let res = diff(&repo, args()).unwrap();
        assert_eq!(res.unmerged, ["a.txt"]);
        assert!(res.files.is_empty());
        let mut a = args();
        a.staged = true;
        let res = diff(&repo, a).unwrap();
        assert_eq!(res.unmerged, ["a.txt"]);
        assert!(res.files.is_empty());
    }

    #[test]
    fn argument_errors_are_actionable() {
        let (_dir, repo) = fixture_repo();
        let msg = |a: DiffArgs| format!("{:#}", diff(&repo, a).unwrap_err());
        let mut a = revs("HEAD~1", "HEAD");
        a.staged = true;
        assert!(msg(a).contains("can't be combined"));
        let mut a = args();
        a.to = Some("HEAD".into());
        assert!(msg(a).contains("needs `from`"));
        let mut a = args();
        a.from = Some("HEAD".into());
        assert!(msg(a).contains("staged: true"));
        assert!(msg(revs("nope", "HEAD")).contains("cannot resolve `nope`"));

        let (_dir, bare) = bare_repo();
        assert!(format!("{:#}", diff(&bare, args()).unwrap_err()).contains("bare"));
        let res = diff(&bare, revs("HEAD~1", "HEAD")).unwrap();
        assert_eq!(res.files.len(), 1);
    }

    #[test]
    fn quotes_paths_git_would_quote() {
        assert_eq!(
            quote_path("a/", "plain name.txt".into()),
            "a/plain name.txt"
        );
        assert_eq!(quote_path("a/", "tab\there".into()), "\"a/tab\\there\"");
    }
}
