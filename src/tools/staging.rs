use std::{collections::BTreeMap, io::Read as _, path::Path};

use anyhow::{Context, bail};
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    dir::{
        entry::{Kind as DiskKind, PathspecMatch, Status as DirStatus},
        walk::EmissionMode,
    },
    filter::plumbing::pipeline::convert::ToGitOutcome,
    index::entry::{Flags, Mode, Stage, Stat},
    status::{
        UntrackedFiles, index_worktree,
        plumbing::index_as_worktree::{Change, EntryStatus},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ChangeKind,
    diff::index_changes,
    index_entry_kind, pathspec_patterns, pathspec_search,
    write::{ensure_idle, require_worktree, update_index},
};

/// Changed paths listed per call; `changed_count` always has the full number.
const MAX_LISTED: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StageArgs {
    /// Paths or pathspec patterns to stage, relative to the repository root (e.g. `src/main.rs`, `src/`,
    /// `*.rs`). Modified, deleted and new files that match are staged, like `git add <paths>`.
    /// Required unless `all` is set.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Stage every change in the working tree, including deletions and new untracked files, like
    /// `git add -A` (default false).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnstageArgs {
    /// Paths or pathspec patterns whose staged changes to undo, relative to the repository root.
    /// Required unless `all` is set.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Unstage everything, like `git reset` with no paths (default false).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexChange {
    /// Path relative to the repository root.
    pub path: String,
    /// How the index entry changed: `added`, `modified`, `deleted` or `typechange` for `stage`; for
    /// `unstage`, the kind of staged change that was undone.
    pub change: ChangeKind,
    /// True when the path had merge conflicts that staging it resolved.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub resolved_conflict: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkippedPath {
    /// Path relative to the repository root.
    pub path: String,
    /// Why it wasn't staged.
    pub reason: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IndexUpdateResult {
    /// What this call changed in the index, ordered by path; at most 500 entries.
    pub changed: Vec<IndexChange>,
    /// Number of paths this call changed, including any beyond the listed 500.
    pub changed_count: usize,
    /// Paths that matched but were left alone: ignored files named explicitly (git refuses those without
    /// `--force`) and nested repositories (add those as submodules with git).
    pub skipped: Vec<SkippedPath>,
    /// Number of paths that now differ between HEAD and the index: what `commit` would record.
    pub staged_total: usize,
}

/// Stage working-tree changes, like `git add <paths>` or `git add -A`.
pub fn stage(repo: &gix::Repository, args: StageArgs) -> anyhow::Result<IndexUpdateResult> {
    let workdir = require_worktree(repo, "stage")?.to_owned();
    let paths = selection(&args.paths, args.all, "stage")?;
    let (mut changed, skipped) = update_index(repo, |index| {
        let plan = plan_stage(repo, &paths)?;
        if plan.updates.is_empty() && plan.skipped.is_empty() {
            ensure_matches(repo, index, &workdir, &paths)?;
        }
        let changed = apply_stage(repo, index, &workdir, plan.updates)?;
        Ok((changed, plan.skipped))
    })?;
    finish(repo, &mut changed, skipped)
}

/// Undo staged changes, resetting index entries to HEAD, like `git restore --staged <paths>`.
pub fn unstage(repo: &gix::Repository, args: UnstageArgs) -> anyhow::Result<IndexUpdateResult> {
    require_worktree(repo, "unstage")?;
    ensure_idle(repo, "unstage changes")?;
    let paths = selection(&args.paths, args.all, "unstage")?;
    let head_tree = repo.head_tree_id_or_empty()?.detach();
    let mut changed = update_index(repo, |index| reset_entries(repo, index, head_tree, &paths))?;
    finish(repo, &mut changed, Vec::new())
}

/// The pathspecs to act on: `paths`, or everything with `all`.
fn selection(paths: &[String], all: bool, verb: &str) -> anyhow::Result<Vec<String>> {
    match (paths.is_empty(), all) {
        (true, false) => {
            bail!("pass the `paths` to {verb}, or set `all: true` to {verb} everything")
        }
        (false, true) => bail!("pass either `paths` or `all: true`, not both"),
        (false, false) => Ok(paths.to_vec()),
        (true, true) => Ok(Vec::new()),
    }
}

fn finish(
    repo: &gix::Repository,
    changed: &mut Vec<IndexChange>,
    skipped: Vec<SkippedPath>,
) -> anyhow::Result<IndexUpdateResult> {
    changed.sort_by(|a, b| a.path.cmp(&b.path));
    let changed_count = changed.len();
    changed.truncate(MAX_LISTED);
    let head_tree = repo.head_tree_id_or_empty()?;
    let (staged, unmerged) = index_changes(repo, &head_tree, &[])?;
    Ok(IndexUpdateResult {
        changed: std::mem::take(changed),
        changed_count,
        skipped,
        staged_total: staged.len() + unmerged.len(),
    })
}

/// What staging will do, worked out from the index-to-worktree status before anything is written.
#[derive(Default)]
struct StagePlan {
    /// Paths to stage, with whether they're conflicted; `None` contents means the file was deleted.
    updates: BTreeMap<BString, Update>,
    skipped: Vec<SkippedPath>,
}

#[derive(Clone, Copy, Default)]
struct Update {
    deleted: bool,
    conflicted: bool,
    /// For a submodule: the commit now checked out in it.
    submodule_head: Option<ObjectId>,
}

fn plan_stage(repo: &gix::Repository, paths: &[String]) -> anyhow::Result<StagePlan> {
    let explicit = !paths.is_empty();
    let status = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Files)
        // Ignored files are only reported so an explicitly named one can be explained.
        .dirwalk_options(|opts| opts.emit_ignored(explicit.then_some(EmissionMode::Matching)))
        .into_index_worktree_iter(pathspec_patterns(paths))?;
    let mut plan = StagePlan::default();
    for item in status {
        match item? {
            index_worktree::Item::Modification {
                rela_path, status, ..
            } => {
                let update = match status {
                    EntryStatus::Conflict { .. } => Update {
                        conflicted: true,
                        ..Update::default()
                    },
                    EntryStatus::Change(Change::Removed) => Update {
                        deleted: true,
                        ..Update::default()
                    },
                    EntryStatus::Change(Change::SubmoduleModification(sub)) => {
                        // Only a new commit checked out in the submodule changes what git records.
                        match sub.checked_out_head_id {
                            Some(head) if Some(head) != sub.index_id => Update {
                                submodule_head: Some(head),
                                ..Update::default()
                            },
                            _ => continue,
                        }
                    }
                    EntryStatus::Change(_) | EntryStatus::IntentToAdd => Update::default(),
                    EntryStatus::NeedsUpdate(_) => continue,
                };
                let slot = plan.updates.entry(rela_path).or_default();
                slot.conflicted |= update.conflicted;
                slot.deleted |= update.deleted;
                slot.submodule_head = slot.submodule_head.or(update.submodule_head);
            }
            index_worktree::Item::DirectoryContents { entry, .. } => {
                record_untracked(&mut plan, entry);
            }
            index_worktree::Item::Rewrite {
                source,
                dirwalk_entry,
                ..
            } => {
                plan.updates.insert(
                    source.rela_path().to_owned(),
                    Update {
                        deleted: true,
                        ..Update::default()
                    },
                );
                record_untracked(&mut plan, dirwalk_entry);
            }
        }
    }
    Ok(plan)
}

fn record_untracked(plan: &mut StagePlan, entry: gix::dir::Entry) {
    let path = entry.rela_path.to_string();
    match (entry.status, entry.disk_kind) {
        (DirStatus::Ignored(_), _) if entry.pathspec_match == Some(PathspecMatch::Verbatim) => {
            plan.skipped.push(SkippedPath {
                path,
                reason: "ignored by a .gitignore rule; git only stages it with `git add --force`"
                    .into(),
            });
        }
        (DirStatus::Untracked, Some(DiskKind::Repository)) => plan.skipped.push(SkippedPath {
            path,
            reason: "a nested git repository; add it as a submodule with git instead".into(),
        }),
        (DirStatus::Untracked, Some(DiskKind::File | DiskKind::Symlink)) => {
            plan.updates.insert(entry.rela_path, Update::default());
        }
        _ => {}
    }
}

/// Refuse a call whose pathspecs match nothing tracked or on disk, as `git add` does.
fn ensure_matches(
    repo: &gix::Repository,
    index: &gix::index::State,
    workdir: &Path,
    paths: &[String],
) -> anyhow::Result<()> {
    for path in paths {
        let mut spec = pathspec_search(repo, std::slice::from_ref(path), index)?
            .context("a pathspec was given")?;
        let tracked = index
            .entries()
            .iter()
            .any(|e| spec.is_included(e.path(index), Some(false)));
        if !tracked && std::fs::symlink_metadata(workdir.join(path)).is_err() {
            bail!(
                "`{path}` matches no file in the index or working tree; check it with status or list_tree"
            );
        }
    }
    Ok(())
}

fn apply_stage(
    repo: &gix::Repository,
    index: &mut gix::index::State,
    workdir: &Path,
    updates: BTreeMap<BString, Update>,
) -> anyhow::Result<Vec<IndexChange>> {
    let filemode = filemode(repo);
    let (mut pipeline, _) = repo.filter_pipeline(None)?;
    let mut changed = Vec::new();
    let mut replacements = Vec::new();
    for (path, update) in updates {
        let previous = index
            .entry_by_path_and_stage(path.as_bstr(), Stage::Unconflicted)
            .or_else(|| {
                // A conflicted path has no stage 0; compare against our side.
                index.entry_by_path_and_stage(path.as_bstr(), Stage::Ours)
            })
            .map(|e| e.mode);
        let new = if update.deleted {
            None
        } else if let Some(head) = update.submodule_head {
            Some((head, Mode::COMMIT, Stat::default()))
        } else {
            worktree_blob(
                repo,
                &mut pipeline,
                index,
                workdir,
                path.as_bstr(),
                previous,
                filemode,
            )?
        };
        let change = match (previous, &new) {
            (None, None) => continue,
            (None, Some(_)) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Deleted,
            (Some(old), Some((_, mode, _))) => {
                ChangeKind::of_modification(index_entry_kind(old), index_entry_kind(*mode))
            }
        };
        changed.push(IndexChange {
            path: path.to_string(),
            change,
            resolved_conflict: update.conflicted,
        });
        replacements.push((path, new));
    }
    replace_entries(index, replacements);
    Ok(changed)
}

/// A path and the stage-0 entry that replaces all of its index entries; `None` drops them.
pub type Replacement = (BString, Option<(ObjectId, Mode, Stat)>);

/// Apply `replacements` in one pass over the index, however many paths change.
pub fn replace_entries(index: &mut gix::index::State, replacements: Vec<Replacement>) {
    let gone: std::collections::HashSet<&BStr> = replacements
        .iter()
        .map(|(path, _)| path.as_bstr())
        .collect();
    index.remove_entries(|_, path, _| gone.contains(path));
    for (path, new) in &replacements {
        if let Some((id, mode, stat)) = new {
            index.dangerously_push_entry(*stat, *id, Flags::empty(), *mode, path.as_bstr());
        }
    }
}

/// Hash the file at `path` into a blob as git would store it: clean filters and line-ending
/// conversion from `.gitattributes` applied, symlinks stored as their target. `None` when it's gone.
pub fn worktree_blob(
    repo: &gix::Repository,
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &gix::index::State,
    workdir: &Path,
    path: &BStr,
    previous: Option<Mode>,
    filemode: bool,
) -> anyhow::Result<Option<(ObjectId, Mode, Stat)>> {
    let rela = gix::path::from_bstr(path)?;
    let full = workdir.join(&rela);
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("cannot read `{path}`")),
    };
    let (data, mode) = if meta.is_symlink() {
        let target = std::fs::read_link(&full)?;
        (gix::path::into_bstr(target)?.to_vec(), Mode::SYMLINK)
    } else if meta.is_file() {
        let file = std::fs::File::open(&full).with_context(|| format!("cannot read `{path}`"))?;
        let data = match pipeline.convert_to_git(file, &rela, index)? {
            ToGitOutcome::Unchanged(mut file) => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                buf
            }
            ToGitOutcome::Buffer(buf) => buf.to_vec(),
            ToGitOutcome::Process(mut reader) => {
                let mut buf = Vec::new();
                reader.read_to_end(&mut buf)?;
                buf
            }
        };
        (data, file_mode(&meta, previous, filemode))
    } else {
        return Ok(None);
    };
    let id = repo.write_blob(&data)?.detach();
    let stat = Stat::from_fs(&gix::index::fs::Metadata::from_path_no_follow(&full)?)?;
    Ok(Some((id, mode, stat)))
}

/// Whether the filesystem's executable bit counts (`core.fileMode`, default true).
pub fn filemode(repo: &gix::Repository) -> bool {
    repo.config_snapshot()
        .boolean("core.fileMode")
        .unwrap_or(true)
}

/// The executable bit comes from disk unless `core.fileMode` is off, in which case the index keeps
/// what it had, as git does on filesystems without reliable permissions.
fn file_mode(meta: &std::fs::Metadata, previous: Option<Mode>, filemode: bool) -> Mode {
    if !filemode {
        return match previous {
            Some(Mode::FILE_EXECUTABLE) => Mode::FILE_EXECUTABLE,
            _ => Mode::FILE,
        };
    }
    if is_executable(meta) {
        Mode::FILE_EXECUTABLE
    } else {
        Mode::FILE
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// A file version as the index records it: blob id (or a submodule's commit) and mode.
pub type Version = (ObjectId, Mode);

/// Every file, symlink and submodule in `tree`, by path.
pub fn tree_versions(
    repo: &gix::Repository,
    tree: ObjectId,
) -> anyhow::Result<BTreeMap<BString, Version>> {
    let mut recorder = gix::traverse::tree::Recorder::default();
    repo.find_tree(tree)?
        .traverse()
        .breadthfirst(&mut recorder)?;
    Ok(recorder
        .records
        .into_iter()
        .filter(|record| !record.mode.is_tree())
        .map(|record| (record.filepath, (record.oid, record.mode.into())))
        .collect())
}

/// Point every index entry matching `paths` (everything when empty) back at `tree`'s version,
/// dropping entries `tree` doesn't have. Returns the staged changes that were undone.
pub fn reset_entries(
    repo: &gix::Repository,
    index: &mut gix::index::State,
    tree: ObjectId,
    paths: &[String],
) -> anyhow::Result<Vec<IndexChange>> {
    let mut spec = pathspec_search(repo, paths, index)?;
    let mut included = |path: &BStr| {
        spec.as_mut()
            .is_none_or(|s| s.is_included(path, Some(false)))
    };
    let mut wanted = tree_versions(repo, tree)?;
    wanted.retain(|path, _| included(path.as_bstr()));
    let mut current: BTreeMap<BString, (ObjectId, Mode, bool)> = BTreeMap::new();
    for entry in index.entries() {
        let path = entry.path(index);
        if included(path) {
            let slot = current
                .entry(path.to_owned())
                .or_insert((entry.id, entry.mode, false));
            slot.2 |= entry.stage() != Stage::Unconflicted;
        }
    }

    let mut changed = Vec::new();
    let mut replacements = Vec::new();
    let paths: std::collections::BTreeSet<_> =
        wanted.keys().chain(current.keys()).cloned().collect();
    for path in paths {
        let target = wanted.get(&path).copied();
        let existing = current.get(&path).copied();
        let change = match (existing, target) {
            (Some((id, mode, false)), Some(want)) if (id, mode) == want => continue,
            (None, None) => continue,
            (None, Some(_)) => ChangeKind::Deleted,
            (Some(_), None) => ChangeKind::Added,
            (Some((_, mode, _)), Some((_, want_mode))) => {
                ChangeKind::of_modification(index_entry_kind(want_mode), index_entry_kind(mode))
            }
        };
        changed.push(IndexChange {
            path: path.to_string(),
            change,
            resolved_conflict: false,
        });
        // A zero stat makes git re-read the file instead of trusting a stat that matched the entry
        // this one replaces.
        replacements.push((path, target.map(|(id, mode)| (id, mode, Stat::default()))));
    }
    replace_entries(index, replacements);
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{
        assert_unchanged, bare_repo, empty_repo, fixture_repo, git_stdout, ls_files, open,
        porcelain, run_git, snapshot, try_git, twins, write,
    };

    fn stage_paths(repo: &gix::Repository, paths: &[&str]) -> anyhow::Result<IndexUpdateResult> {
        stage(
            repo,
            StageArgs {
                paths: paths.iter().map(|p| p.to_string()).collect(),
                all: false,
            },
        )
    }

    fn stage_all(repo: &gix::Repository) -> anyhow::Result<IndexUpdateResult> {
        stage(
            repo,
            StageArgs {
                paths: vec![],
                all: true,
            },
        )
    }

    fn unstage_paths(repo: &gix::Repository, paths: &[&str]) -> anyhow::Result<IndexUpdateResult> {
        unstage(
            repo,
            UnstageArgs {
                paths: paths.iter().map(|p| p.to_string()).collect(),
                all: false,
            },
        )
    }

    /// Modified, deleted, new (nested and unicode), executable, symlink and ignored files.
    fn edits(root: &Path) {
        write(root, ".gitignore", "*.log\n");
        write(root, "gone.txt", "soon gone\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "more files"]);
        write(root, "a.txt", "one\ntwo\nthree\n");
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        write(root, "src/new/deep.rs", "fn deep() {}\n");
        write(root, "dïr/ünï.txt", "unicode\n");
        write(root, "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(
            root.join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        write(root, "debug.log", "ignored\n");
    }

    #[test]
    fn stage_all_matches_git_add_all() {
        let [(ours, repo), (theirs, _)] = twins(edits);
        let res = stage_all(&repo).unwrap();
        run_git(theirs.path(), &["add", "-A"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(porcelain(ours.path()), porcelain(theirs.path()));
        assert!(try_git(ours.path(), &["fsck", "--strict"]));

        let changes: Vec<_> = res
            .changed
            .iter()
            .map(|c| (c.path.as_str(), c.change))
            .collect();
        assert_eq!(
            changes,
            [
                ("a.txt", ChangeKind::Modified),
                ("dïr/ünï.txt", ChangeKind::Added),
                ("gone.txt", ChangeKind::Deleted),
                ("link", ChangeKind::Added),
                ("run.sh", ChangeKind::Added),
                ("src/new/deep.rs", ChangeKind::Added),
            ]
        );
        assert_eq!(res.changed_count, 6);
        assert!(
            res.skipped.is_empty(),
            "ignored files under `all` aren't worth reporting"
        );
        let staged = git_stdout(ours.path(), &["diff", "--cached", "--name-only"]);
        assert_eq!(res.staged_total, staged.lines().count());
        assert_eq!(
            git_stdout(ours.path(), &["diff", "--name-only"]),
            "",
            "the worktree is clean after staging"
        );
    }

    #[test]
    fn stage_paths_matches_git_add() {
        let [(ours, repo), (theirs, _)] = twins(edits);
        let res = stage_paths(&repo, &["src/", "*.sh", "gone.txt", "debug.log"]).unwrap();
        run_git(theirs.path(), &["add", "src/", "*.sh", "gone.txt"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(res.skipped.len(), 1);
        assert_eq!(res.skipped[0].path, "debug.log");
        assert!(res.skipped[0].reason.contains("--force"));
    }

    #[test]
    fn staging_resolves_conflicts() {
        let conflicted = |root: &Path| {
            run_git(root, &["checkout", "-q", "feature"]);
            write(root, "a.txt", "theirs\n");
            run_git(root, &["commit", "-q", "-am", "theirs"]);
            run_git(root, &["checkout", "-q", "main"]);
            write(root, "a.txt", "ours\n");
            run_git(root, &["commit", "-q", "-am", "ours"]);
            assert!(!try_git(root, &["merge", "-q", "feature"]));
            write(root, "a.txt", "resolved\n");
        };
        let [(ours, repo), (theirs, _)] = twins(conflicted);
        let res = stage_paths(&repo, &["a.txt"]).unwrap();
        run_git(theirs.path(), &["add", "a.txt"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert!(res.changed[0].resolved_conflict);
        assert_eq!(git_stdout(ours.path(), &["ls-files", "-u"]), "");
    }

    #[test]
    fn line_endings_and_file_mode_follow_config() {
        let crlf = |root: &Path| {
            write(root, ".gitattributes", "*.txt text eol=lf\n");
            run_git(root, &["add", ".gitattributes"]);
            run_git(root, &["commit", "-q", "-m", "attributes"]);
            write(root, "w.txt", "windows\r\nlines\r\n");
            write(root, "a.txt", "one\r\ntwo\r\n");
        };
        let [(ours, repo), (theirs, _)] = twins(crlf);
        stage_all(&repo).unwrap();
        run_git(theirs.path(), &["add", "-A"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(
            git_stdout(ours.path(), &["show", ":w.txt"]),
            "windows\nlines\n"
        );

        let no_filemode = |root: &Path| {
            run_git(root, &["config", "core.fileMode", "false"]);
            std::fs::set_permissions(
                root.join("a.txt"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            write(root, "a.txt", "changed\n");
        };
        let [(ours, repo), (theirs, _)] = twins(no_filemode);
        stage_all(&repo).unwrap();
        run_git(theirs.path(), &["add", "-A"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert!(ls_files(ours.path()).starts_with("100644"));
    }

    #[test]
    fn submodule_commits_are_staged() {
        let (sub, _) = fixture_repo();
        let with_submodule = |root: &Path| {
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
            run_git(&root.join("sub"), &["checkout", "-q", "HEAD~1"]);
        };
        let [(ours, repo), (theirs, _)] = twins(with_submodule);
        let res = stage_all(&repo).unwrap();
        run_git(theirs.path(), &["add", "-A"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(res.changed[0].path, "sub");
    }

    #[test]
    fn nested_repositories_dirty_submodules_and_clean_paths() {
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
        write(&root.join("sub"), "a.txt", "dirty, but no new commit\n");
        std::fs::create_dir(root.join("nested")).unwrap();
        run_git(&root.join("nested"), &["init", "-q"]);
        write(&root.join("nested"), "inner.txt", "inner\n");
        let repo = open(root);
        let res = stage_all(&repo).unwrap();
        assert!(
            res.changed.is_empty(),
            "a dirty submodule records nothing new: {:?}",
            res.changed.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
        assert_eq!(res.skipped.len(), 1);
        assert_eq!(res.skipped[0].path, "nested");
        assert!(res.skipped[0].reason.contains("submodule"));

        // A pathspec naming an unchanged tracked file is fine; there's just nothing to do.
        let clean = stage_paths(&repo, &["a.txt"]).unwrap();
        assert!(clean.changed.is_empty());
    }

    #[test]
    fn split_indexes_keep_every_entry() {
        let split = |root: &Path| {
            edits(root);
            run_git(root, &["update-index", "--split-index"]);
        };
        let [(ours, repo), (theirs, _)] = twins(split);
        stage_all(&repo).unwrap();
        run_git(theirs.path(), &["add", "-A"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert!(try_git(ours.path(), &["fsck", "--strict"]));
    }

    #[test]
    fn sparse_indexes_are_refused() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "inside/a.txt", "in\n");
        write(root, "outside/b.txt", "out\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "two directories"]);
        run_git(
            root,
            &[
                "sparse-checkout",
                "set",
                "--cone",
                "--sparse-index",
                "inside",
            ],
        );
        write(root, "inside/a.txt", "edited\n");
        let repo = open(root);
        let before = snapshot(root);
        let err = stage_all(&repo).unwrap_err().to_string();
        assert!(err.contains("sparse index"), "{err}");
        assert_unchanged(root, &before);
    }

    #[test]
    fn stage_in_a_fresh_repository() {
        let (dir, repo) = empty_repo();
        write(dir.path(), "first.txt", "hello\n");
        let res = stage_all(&repo).unwrap();
        assert_eq!(res.staged_total, 1);
        assert_eq!(
            git_stdout(dir.path(), &["diff", "--cached", "--name-only"]),
            "first.txt\n"
        );
    }

    #[test]
    fn stage_refusals_change_nothing() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        write(root, "a.txt", "edited\n");
        let refused = |needle: &str, f: &dyn Fn() -> anyhow::Result<IndexUpdateResult>| {
            let before = snapshot(root);
            let err = format!("{:#}", f().unwrap_err());
            assert!(err.contains(needle), "expected `{needle}` in: {err}");
            assert_unchanged(root, &before);
        };
        refused("matches no file", &|| stage_paths(&repo, &["nope.txt"]));
        refused("`paths`", &|| {
            stage(
                &repo,
                StageArgs {
                    paths: vec![],
                    all: false,
                },
            )
        });
        refused("not both", &|| {
            stage(
                &repo,
                StageArgs {
                    paths: vec!["a.txt".into()],
                    all: true,
                },
            )
        });
        refused("..", &|| stage_paths(&repo, &["../x"]));
        std::fs::write(root.join(".git/index.lock"), "").unwrap();
        refused("index.lock", &|| stage_all(&repo));
        std::fs::remove_file(root.join(".git/index.lock")).unwrap();

        let (_dir, bare) = bare_repo();
        assert!(stage_all(&bare).unwrap_err().to_string().contains("bare"));
    }

    #[test]
    fn unstage_matches_git_restore_staged() {
        let staged = |root: &Path| {
            edits(root);
            run_git(root, &["add", "-A"]);
        };
        let [(ours, repo), (theirs, _)] = twins(staged);
        let worktree_before = snapshot(&ours.path().join("src"));
        let res = unstage_paths(&repo, &["a.txt", "gone.txt", "src/"]).unwrap();
        run_git(
            theirs.path(),
            &["restore", "--staged", "a.txt", "gone.txt", "src/"],
        );
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(
            porcelain(ours.path()),
            porcelain(theirs.path()),
            "git sees the same unstaged changes"
        );
        assert_eq!(
            snapshot(&ours.path().join("src")),
            worktree_before,
            "the worktree is untouched"
        );
        let changes: Vec<_> = res
            .changed
            .iter()
            .map(|c| (c.path.as_str(), c.change))
            .collect();
        assert_eq!(
            changes,
            [
                ("a.txt", ChangeKind::Modified),
                ("gone.txt", ChangeKind::Deleted),
                ("src/new/deep.rs", ChangeKind::Added),
            ]
        );

        let [(ours, repo), (theirs, _)] = twins(staged);
        unstage(
            &repo,
            UnstageArgs {
                paths: vec![],
                all: true,
            },
        )
        .unwrap();
        run_git(theirs.path(), &["reset", "-q"]);
        assert_eq!(ls_files(ours.path()), ls_files(theirs.path()));
        assert_eq!(porcelain(ours.path()), porcelain(theirs.path()));
    }

    #[test]
    fn unstage_in_a_fresh_repository_and_refusals() {
        let (dir, repo) = empty_repo();
        write(dir.path(), "first.txt", "hello\n");
        run_git(dir.path(), &["add", "."]);
        let res = unstage(
            &repo,
            UnstageArgs {
                paths: vec![],
                all: true,
            },
        )
        .unwrap();
        assert_eq!(res.changed[0].change, ChangeKind::Added);
        assert_eq!(ls_files(dir.path()), "");

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
        let before = snapshot(root);
        let err = unstage_paths(&repo, &["a.txt"]).unwrap_err().to_string();
        assert!(err.contains("in progress"), "{err}");
        assert_unchanged(root, &before);
    }
}
