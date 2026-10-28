use gix::{
    dir::{entry::Kind as DiskKind, entry::Status as DirStatus, walk::EmissionMode},
    status::{
        UntrackedFiles, index_worktree,
        plumbing::index_as_worktree::{Change, EntryStatus},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ChangeKind, ConflictKind, diff::index_changes, page_limit, pathspec_patterns};

const DEFAULT_STATUS_LIMIT: usize = 500;
const MAX_STATUS_LIMIT: usize = 5000;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusArgs {
    /// Limit to these paths or git pathspec patterns (e.g. `src/`, `*.rs`, `:(exclude)docs`),
    /// relative to the repository root. Omit or pass `[]` for the whole repository.
    #[serde(default)]
    pub pathspec: Vec<String>,
    /// List untracked files (default true). Untracked directories are collapsed to `dir/`, like `git status`.
    #[serde(default = "default_true")]
    pub include_untracked: bool,
    /// List ignored files in `ignored` (default false). Ignored directories are collapsed to `dir/`.
    #[serde(default)]
    pub include_ignored: bool,
    /// Maximum entries returned in each list (default 500, max 5000). `counts` always has the full totals.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusEntry {
    /// Path relative to the repository root (the new path for renames).
    pub path: String,
    /// How the path changed.
    pub change: ChangeKind,
    /// Previous path for `renamed` and `copied`; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

impl StatusEntry {
    fn new(path: impl ToString, change: ChangeKind) -> Self {
        Self {
            path: path.to_string(),
            change,
            old_path: None,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConflictEntry {
    /// Path relative to the repository root.
    pub path: String,
    /// Which sides of the merge changed or deleted the path.
    pub conflict: ConflictKind,
}

/// Full list sizes, before `limit` was applied.
#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct StatusCounts {
    /// Entries in the full `staged` list.
    pub staged: usize,
    /// Entries in the full `unstaged` list.
    pub unstaged: usize,
    /// Entries in the full `untracked` list.
    pub untracked: usize,
    /// Entries in the full `ignored` list; 0 unless `include_ignored` is set.
    pub ignored: usize,
    /// Entries in the full `conflicted` list.
    pub conflicted: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusResult {
    /// Short name of the current branch; `null` when HEAD is detached.
    pub branch: Option<String>,
    /// Full SHA of the commit HEAD points to; `null` when the branch has no commits yet.
    pub head: Option<String>,
    /// Upstream tracking ref of the current branch from local config (e.g. `refs/remotes/origin/main`),
    /// or `null` if none is configured. Never fetched, so it may be stale.
    pub upstream: Option<String>,
    /// Changes between HEAD and the index: what `git commit` would record.
    pub staged: Vec<StatusEntry>,
    /// Changes between the index and the working tree that are not staged yet.
    pub unstaged: Vec<StatusEntry>,
    /// Untracked paths; directories end in `/`.
    pub untracked: Vec<String>,
    /// Ignored paths when `include_ignored` is set; directories end in `/`.
    pub ignored: Vec<String>,
    /// Paths with unresolved merge conflicts. These are not repeated in `staged` or `unstaged`.
    pub conflicted: Vec<ConflictEntry>,
    /// Size of each list before `limit` was applied.
    pub counts: StatusCounts,
    /// True if any list was cut off at `limit`; narrow with `pathspec` to see the rest.
    pub truncated: bool,
    /// True when nothing is staged, unstaged, conflicted or untracked. Ignored files don't count.
    pub clean: bool,
}

pub fn status(repo: &gix::Repository, args: StatusArgs) -> anyhow::Result<StatusResult> {
    if repo.workdir().is_none() {
        anyhow::bail!(
            "status needs a working tree, but this repository is bare; use log, or diff with `from` and `to`"
        );
    }
    let mut lists = Lists::default();
    // The HEAD-to-index side is the same comparison `diff` makes for staged changes.
    let head_tree = repo.head_tree_id_or_empty()?;
    let (staged, _) = index_changes(repo, &head_tree, &args.pathspec)?;
    lists.staged = staged
        .iter()
        .map(|c| StatusEntry {
            path: c.path().to_string(),
            change: c.status,
            old_path: c.old_path().map(|p| p.to_string()),
        })
        .collect();
    let worktree =
        configure(repo, &args)?.into_index_worktree_iter(pathspec_patterns(&args.pathspec))?;
    for item in worktree {
        lists.record_worktree(item?, &args);
    }
    lists.sort_and_dedup();

    let counts = lists.counts();
    let clean = counts.staged + counts.unstaged + counts.untracked + counts.conflicted == 0;
    let limit = page_limit(args.limit, DEFAULT_STATUS_LIMIT, MAX_STATUS_LIMIT);
    let mut truncated = false;
    Ok(StatusResult {
        branch: repo.head_name()?.map(|n| n.shorten().to_string()),
        head: repo.head_id().ok().map(|id| id.to_string()),
        upstream: upstream(repo),
        staged: first_n(lists.staged, limit, &mut truncated),
        unstaged: first_n(lists.unstaged, limit, &mut truncated),
        untracked: first_n(lists.untracked, limit, &mut truncated),
        ignored: first_n(lists.ignored, limit, &mut truncated),
        conflicted: first_n(lists.conflicted, limit, &mut truncated),
        counts,
        truncated,
        clean,
    })
}

/// Set up the status walk. Untracked handling follows `status.showUntrackedFiles` unless disabled.
fn configure<'repo>(
    repo: &'repo gix::Repository,
    args: &StatusArgs,
) -> anyhow::Result<gix::status::Platform<'repo, gix::progress::Discard>> {
    let platform = repo.status(gix::progress::Discard)?;
    // The directory walk finds both untracked and ignored files, so keep it if either is wanted
    // and filter untracked entries out afterwards when only ignored ones were requested.
    if !args.include_untracked && !args.include_ignored {
        return Ok(platform.untracked_files(UntrackedFiles::None));
    }
    let ignored = args
        .include_ignored
        .then_some(EmissionMode::CollapseDirectory);
    Ok(platform.dirwalk_options(|opts| opts.emit_ignored(ignored)))
}

fn first_n<T>(mut list: Vec<T>, limit: usize, truncated: &mut bool) -> Vec<T> {
    if list.len() > limit {
        list.truncate(limit);
        *truncated = true;
    }
    list
}

/// The configured upstream is auxiliary information: a broken remote config shouldn't fail status.
fn upstream(repo: &gix::Repository) -> Option<String> {
    let head = repo.head_name().ok()??;
    match repo.branch_remote_tracking_ref_name(head.as_ref(), gix::remote::Direction::Fetch)? {
        Ok(name) => Some(name.as_bstr().to_string()),
        Err(err) => {
            tracing::debug!(%err, "could not resolve upstream");
            None
        }
    }
}

#[derive(Default)]
struct Lists {
    staged: Vec<StatusEntry>,
    unstaged: Vec<StatusEntry>,
    untracked: Vec<String>,
    ignored: Vec<String>,
    conflicted: Vec<ConflictEntry>,
}

impl Lists {
    fn record_worktree(&mut self, item: index_worktree::Item, args: &StatusArgs) {
        match item {
            index_worktree::Item::Modification {
                rela_path, status, ..
            } => match status {
                EntryStatus::Conflict { summary, .. } => self.conflicted.push(ConflictEntry {
                    path: rela_path.to_string(),
                    conflict: summary.into(),
                }),
                EntryStatus::Change(change) => self
                    .unstaged
                    .push(StatusEntry::new(rela_path, worktree_change_kind(&change))),
                EntryStatus::IntentToAdd => self
                    .unstaged
                    .push(StatusEntry::new(rela_path, ChangeKind::Added)),
                // Only a stat-cache refresh; the content is unchanged.
                EntryStatus::NeedsUpdate(_) => {}
            },
            index_worktree::Item::DirectoryContents { entry, .. } => {
                let path = dir_entry_path(&entry);
                match entry.status {
                    DirStatus::Untracked if args.include_untracked => self.untracked.push(path),
                    DirStatus::Ignored(_) => self.ignored.push(path),
                    _ => {}
                }
            }
            // Index-to-worktree rename tracking is off (as in `git status`), but stay total.
            index_worktree::Item::Rewrite {
                source,
                dirwalk_entry,
                copy,
                ..
            } => self.unstaged.push(StatusEntry {
                path: dirwalk_entry.rela_path.to_string(),
                change: if copy {
                    ChangeKind::Copied
                } else {
                    ChangeKind::Renamed
                },
                old_path: Some(source.rela_path().to_string()),
            }),
        }
    }

    /// The parallel status walk yields items in no particular order, and may report a conflicted
    /// path once per conflict stage.
    fn sort_and_dedup(&mut self) {
        self.conflicted.sort_by(|a, b| a.path.cmp(&b.path));
        self.conflicted.dedup_by(|a, b| a.path == b.path);
        self.staged.sort_by(|a, b| a.path.cmp(&b.path));
        self.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        self.untracked.sort();
        self.ignored.sort();
    }

    fn counts(&self) -> StatusCounts {
        StatusCounts {
            staged: self.staged.len(),
            unstaged: self.unstaged.len(),
            untracked: self.untracked.len(),
            ignored: self.ignored.len(),
            conflicted: self.conflicted.len(),
        }
    }
}

fn worktree_change_kind<T, U>(change: &Change<T, U>) -> ChangeKind {
    match change {
        Change::Removed => ChangeKind::Deleted,
        Change::Type { .. } => ChangeKind::Typechange,
        Change::Modification { .. } | Change::SubmoduleModification(_) => ChangeKind::Modified,
    }
}

/// Collapsed directories are reported with a trailing `/`, as `git status` prints them.
fn dir_entry_path(entry: &gix::dir::Entry) -> String {
    let path = entry.rela_path.to_string();
    match entry.disk_kind {
        Some(DiskKind::Directory | DiskKind::Repository) => format!("{path}/"),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::tools::testutil::{
        bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git, try_git, write,
    };

    fn args() -> StatusArgs {
        StatusArgs {
            pathspec: vec![],
            include_untracked: true,
            include_ignored: false,
            limit: None,
        }
    }

    /// `(XY code, path, orig_path)` triples from `git status --porcelain=v2`, the oracle for status.
    fn git_status(
        dir: &std::path::Path,
        extra: &[&str],
    ) -> BTreeSet<(String, String, Option<String>)> {
        let mut cmd = vec!["-c", "core.quotePath=false", "status", "--porcelain=v2"];
        cmd.extend_from_slice(extra);
        git_stdout(dir, &cmd)
            .lines()
            .map(|line| {
                let (kind, rest) = line.split_once(' ').unwrap();
                match kind {
                    "1" => {
                        let f: Vec<_> = rest.splitn(8, ' ').collect();
                        (f[0].to_string(), f[7].to_string(), None)
                    }
                    "2" => {
                        let f: Vec<_> = rest.splitn(9, ' ').collect();
                        let (path, orig) = f[8].split_once('\t').unwrap();
                        (f[0].to_string(), path.to_string(), Some(orig.to_string()))
                    }
                    "u" => {
                        let f: Vec<_> = rest.splitn(10, ' ').collect();
                        ("UU".to_string(), f[9].to_string(), None)
                    }
                    "?" => ("??".to_string(), rest.to_string(), None),
                    "!" => ("!!".to_string(), rest.to_string(), None),
                    other => panic!("unexpected porcelain line kind {other}"),
                }
            })
            .collect()
    }

    fn code(change: ChangeKind) -> char {
        match change {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::Renamed => 'R',
            ChangeKind::Copied => 'C',
            ChangeKind::Typechange => 'T',
        }
    }

    /// Fold our result into the same shape as [`git_status`].
    fn ours(res: &StatusResult) -> BTreeSet<(String, String, Option<String>)> {
        let mut by_path: std::collections::BTreeMap<String, (char, char, Option<String>)> =
            Default::default();
        for e in &res.staged {
            let slot = by_path.entry(e.path.clone()).or_insert(('.', '.', None));
            slot.0 = code(e.change);
            slot.2 = e.old_path.clone();
        }
        for e in &res.unstaged {
            by_path.entry(e.path.clone()).or_insert(('.', '.', None)).1 = code(e.change);
        }
        let mut out: BTreeSet<_> = by_path
            .into_iter()
            .map(|(path, (x, y, orig))| (format!("{x}{y}"), path, orig))
            .collect();
        out.extend(
            res.conflicted
                .iter()
                .map(|c| ("UU".to_string(), c.path.clone(), None)),
        );
        out.extend(
            res.untracked
                .iter()
                .map(|p| ("??".to_string(), p.clone(), None)),
        );
        out.extend(
            res.ignored
                .iter()
                .map(|p| ("!!".to_string(), p.clone(), None)),
        );
        out
    }

    #[test]
    fn defaults_from_empty_arguments() {
        let parsed: StatusArgs = serde_json::from_str("{}").unwrap();
        assert!(parsed.include_untracked);
        assert!(!parsed.include_ignored);
        assert!(parsed.pathspec.is_empty() && parsed.limit.is_none());
    }

    #[test]
    fn clean_repo() {
        let (_dir, repo) = fixture_repo();
        let res = status(&repo, args()).unwrap();
        assert!(res.clean);
        assert_eq!(res.branch.as_deref(), Some("main"));
        assert_eq!(res.head, Some(repo.head_id().unwrap().to_string()));
        assert!(res.upstream.is_none());
        assert!(!res.truncated);
    }

    #[test]
    fn matches_git_for_every_change_type() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let git = |args: &[&str]| run_git(root, args);
        write(root, "b.txt", "bee\n");
        write(root, "renamed-src.txt", "line 1\nline 2\nline 3\nline 4\n");
        write(root, "gone.txt", "bye\n");
        write(root, "link-later.txt", "target\n");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "more files"]);

        write(root, "a.txt", "one\ntwo\nthree\n"); // unstaged modification
        write(root, "b.txt", "bee staged\n"); // staged then modified again
        git(&["add", "b.txt"]);
        write(root, "b.txt", "bee staged and changed\n");
        git(&["mv", "renamed-src.txt", "renamed-dst.txt"]); // staged rename
        std::fs::remove_file(root.join("gone.txt")).unwrap(); // unstaged deletion
        write(root, "new-staged.txt", "fresh\n"); // staged addition
        git(&["add", "new-staged.txt"]);
        std::fs::remove_file(root.join("link-later.txt")).unwrap(); // unstaged typechange
        std::os::unix::fs::symlink("a.txt", root.join("link-later.txt")).unwrap();
        write(root, "untracked dir/x.txt", "x\n"); // collapsed untracked directory
        write(root, "ünïcode.txt", "u\n"); // untracked unicode path
        write(root, "intent.txt", "later\n"); // intent-to-add
        git(&["add", "-N", "intent.txt"]);
        git(&["rm", "-q", "--cached", "a.txt"]); // staged deletion, file left untracked on disk

        let res = status(&open(root), args()).unwrap();
        assert_eq!(ours(&res), git_status(root, &[]));
        assert!(!res.clean);
        assert!(res.untracked.contains(&"untracked dir/".to_string()));
        let rename = res
            .staged
            .iter()
            .find(|e| e.change == ChangeKind::Renamed)
            .unwrap();
        assert_eq!(rename.old_path.as_deref(), Some("renamed-src.txt"));
    }

    #[test]
    fn copies_keep_their_modified_source() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        write(root, "orig.txt", &body);
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "orig"]);
        run_git(root, &["config", "status.renames", "copies"]);
        write(root, "copy.txt", &body);
        write(root, "orig.txt", format!("{body}line 21\n"));
        run_git(root, &["add", "."]);
        let res = status(&open(root), args()).unwrap();
        assert_eq!(ours(&res), git_status(root, &[]));
        assert_eq!(res.staged.len(), 2, "{:?}", res.staged);

        // status.renames wins over diff.renames, and `false` turns detection off.
        run_git(root, &["config", "diff.renames", "copies"]);
        run_git(root, &["config", "status.renames", "false"]);
        let res = status(&open(root), args()).unwrap();
        assert_eq!(ours(&res), git_status(root, &[]));
        assert!(res.staged.iter().all(|e| e.old_path.is_none()));
    }

    #[test]
    fn ignored_and_untracked_toggles() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, ".gitignore", "*.log\nbuild/\n");
        write(root, "debug.log", "x");
        write(root, "build/out.o", "x");
        write(root, "notes.md", "x");

        let mut a = args();
        a.include_ignored = true;
        let res = status(&open(root), a).unwrap();
        assert_eq!(ours(&res), git_status(root, &["--ignored"]));
        assert_eq!(res.ignored, ["build/", "debug.log"]);

        let mut a = args();
        a.include_untracked = false;
        a.include_ignored = true;
        let res = status(&open(root), a).unwrap();
        assert!(res.untracked.is_empty());
        assert_eq!(res.counts.ignored, 2);

        let mut a = args();
        a.include_untracked = false;
        let res = status(&open(root), a).unwrap();
        assert!(res.untracked.is_empty() && res.ignored.is_empty());
        assert!(res.clean, "untracked files don't count when not listed");
    }

    #[test]
    fn pathspec_and_limit() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        for name in ["one.rs", "two.rs", "three.md"] {
            write(root, &format!("src/{name}"), "x\n");
        }
        run_git(root, &["add", "src"]);
        let mut a = args();
        a.pathspec = vec!["src/*.rs".into()];
        let res = status(&open(root), a).unwrap();
        let paths: Vec<_> = res.staged.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["src/one.rs", "src/two.rs"]);

        let mut a = args();
        a.limit = Some(2);
        let res = status(&open(root), a).unwrap();
        assert_eq!(res.staged.len(), 2);
        assert_eq!(res.counts.staged, 3);
        assert!(res.truncated);
    }

    #[test]
    fn reports_merge_conflicts_once() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let git = |args: &[&str]| run_git(root, args);
        git(&["checkout", "-q", "feature"]);
        write(root, "a.txt", "feature side\n");
        git(&["commit", "-q", "-am", "feature edit"]);
        git(&["checkout", "-q", "main"]);
        write(root, "a.txt", "main side\n");
        git(&["commit", "-q", "-am", "main edit"]);
        let merge = try_git(root, &["merge", "-q", "feature"]);
        assert!(!merge, "merge should conflict");

        let res = status(&open(root), args()).unwrap();
        assert_eq!(res.conflicted.len(), 1);
        assert_eq!(res.counts.conflicted, 1);
        assert_eq!(res.conflicted[0].path, "a.txt");
        assert_eq!(res.conflicted[0].conflict, ConflictKind::BothModified);
        assert!(res.staged.is_empty() && res.unstaged.is_empty());
        assert_eq!(ours(&res), git_status(root, &[]));
    }

    #[test]
    fn modify_delete_conflict_kind() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        run_git(root, &["rm", "-q", "a.txt"]);
        run_git(root, &["commit", "-q", "-m", "delete a"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "main keeps editing\n");
        run_git(root, &["commit", "-q", "-am", "edit a"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        let res = status(&open(root), args()).unwrap();
        assert_eq!(res.conflicted[0].conflict, ConflictKind::DeletedByThem);
        assert_eq!(ours(&res), git_status(root, &[]));
    }

    #[test]
    fn submodule_with_new_commits_is_modified() {
        let (sub_dir, _) = fixture_repo();
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let sub_url = sub_dir.path().to_str().unwrap();
        run_git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_url,
                "sub",
            ],
        );
        run_git(root, &["commit", "-q", "-m", "add submodule"]);
        write(root, "sub/a.txt", "changed in submodule\n");
        run_git(
            &root.join("sub"),
            &["commit", "-q", "-am", "submodule commit"],
        );

        let res = status(&open(root), args()).unwrap();
        assert_eq!(res.unstaged.len(), 1);
        assert_eq!(res.unstaged[0].path, "sub");
        assert_eq!(res.unstaged[0].change, ChangeKind::Modified);
        assert_eq!(ours(&res), git_status(root, &[]));
    }

    #[test]
    fn detached_head_and_empty_repo() {
        let (dir, _) = fixture_repo();
        run_git(dir.path(), &["checkout", "-q", "--detach", "HEAD~1"]);
        let res = status(&open(dir.path()), args()).unwrap();
        assert!(res.branch.is_none());
        assert!(res.head.is_some());

        let (dir, repo) = empty_repo();
        write(dir.path(), "first.txt", "x");
        let res = status(&repo, args()).unwrap();
        assert_eq!(res.branch.as_deref(), Some("main"));
        assert!(res.head.is_none());
        assert_eq!(res.untracked, ["first.txt"]);
    }

    #[test]
    fn upstream_comes_from_local_config() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run_git(
            root,
            &[
                "config",
                "remote.origin.url",
                "https://example.invalid/repo.git",
            ],
        );
        run_git(
            root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        run_git(root, &["config", "branch.main.remote", "origin"]);
        run_git(root, &["config", "branch.main.merge", "refs/heads/main"]);
        let res = status(&open(root), args()).unwrap();
        assert_eq!(res.upstream.as_deref(), Some("refs/remotes/origin/main"));
    }

    #[test]
    fn bare_repo_is_refused() {
        let (_dir, repo) = bare_repo();
        let err = status(&repo, args()).unwrap_err().to_string();
        assert!(err.contains("bare"), "{err}");
    }
}
