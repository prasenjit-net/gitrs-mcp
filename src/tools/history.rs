use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashSet},
};

use anyhow::Context;
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    date::SecondsSinceUnixEpoch,
    revision::walk::Sorting,
    traverse::commit::simple::CommitTimeOrder,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ChangeKind, LogEntry, Signature, page_limit, paginate_built, progress::Watch, repo_file_path,
    repo_path, resolve_commit,
};

const DEFAULT_LOG_LIMIT: usize = 20;
const MAX_LOG_LIMIT: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogArgs {
    /// Revision to start from (default `HEAD`).
    pub rev: Option<String>,
    /// Maximum commits to return (default 20, max 500).
    pub limit: Option<usize>,
    /// Number of commits to skip first, for pagination.
    pub skip: Option<usize>,
    /// Follow only the first parent of merges.
    #[serde(default)]
    pub first_parent: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LogResult {
    /// Commits newest first.
    pub commits: Vec<LogEntry>,
    /// True if more commits exist past this page.
    pub truncated: bool,
}

/// Commits reachable from `rev`, newest first, like `git log`. Reports commits walked to `watch` and
/// stops when it's cancelled.
pub fn log(repo: &gix::Repository, args: LogArgs, watch: &Watch) -> anyhow::Result<LogResult> {
    let tip = resolve_commit(repo, args.rev.as_deref())?.id;
    let limit = page_limit(args.limit, DEFAULT_LOG_LIMIT, MAX_LOG_LIMIT);

    let mut walk = repo.rev_walk([tip]);
    if args.first_parent {
        walk = walk.first_parent_only();
    }
    let walked = walk.all()?.map(|info| Ok(info?));
    let walked = watch.track(walked, None, "commits walked");
    let (commits, truncated) = paginate_built(walked, args.skip.unwrap_or(0), limit, |info| {
        LogEntry::from_commit(&info.object()?)
    })?;
    Ok(LogResult { commits, truncated })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShowCommitArgs {
    /// Commit to show (default `HEAD`).
    pub rev: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitDetail {
    /// Full commit SHA.
    pub id: String,
    /// Full SHA of the commit's root tree.
    pub tree: String,
    /// Full SHAs of the parent commits; empty for a root commit.
    pub parents: Vec<String>,
    /// Who wrote the change, and when.
    pub author: Signature,
    /// Who created the commit, and when; differs from `author` after rebases and cherry-picks.
    pub committer: Signature,
    /// First line of the commit message.
    pub summary: String,
    /// Full raw message including the summary line.
    pub message: String,
}

pub fn show_commit(repo: &gix::Repository, args: ShowCommitArgs) -> anyhow::Result<CommitDetail> {
    let commit = resolve_commit(repo, args.rev.as_deref())?;
    Ok(CommitDetail {
        id: commit.id.to_string(),
        tree: commit.tree_id()?.to_string(),
        parents: commit.parent_ids().map(|p| p.to_string()).collect(),
        author: Signature::from_ref(commit.author()?)?,
        committer: Signature::from_ref(commit.committer()?)?,
        summary: commit.message()?.summary().to_string(),
        message: commit.message_raw()?.to_string(),
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileHistoryArgs {
    /// File or directory to trace, relative to the repository root.
    pub path: String,
    /// Revision to start from (default `HEAD`).
    pub rev: Option<String>,
    /// Maximum commits to return (default 20, max 500).
    pub limit: Option<usize>,
    /// Number of matching commits to skip first, for pagination.
    pub skip: Option<usize>,
    /// Keep following a single file across renames, like `git log --follow` (default false).
    #[serde(default)]
    pub follow_renames: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileHistoryEntry {
    /// The commit (flattened into this object).
    #[serde(flatten)]
    pub commit: LogEntry,
    /// How the path changed in this commit, compared with its first parent.
    pub change: ChangeKind,
    /// The path as it was in this commit. It differs from the requested path once a rename was followed.
    pub path: String,
    /// The path before this commit, for `renamed`; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileHistoryResult {
    /// Commits that changed the path, newest first.
    pub commits: Vec<FileHistoryEntry>,
    /// True if more matching commits exist past this page.
    pub truncated: bool,
}

/// Commits that changed `path`, newest first, matching `git log -- <path>` (or `--follow`). Reports the
/// commits found to `watch` and stops when it's cancelled.
pub fn file_history(
    repo: &gix::Repository,
    args: FileHistoryArgs,
    watch: &Watch,
) -> anyhow::Result<FileHistoryResult> {
    let path = repo_file_path(&args.path)?;
    let tip = resolve_commit(repo, args.rev.as_deref())?;
    if args.follow_renames && entry_at(&tip, path.as_ref())?.is_some_and(|e| e.mode.is_tree()) {
        anyhow::bail!(
            "follow_renames works on a single file, but `{path}` is a directory; set follow_renames to false"
        );
    }
    let limit = page_limit(args.limit, DEFAULT_LOG_LIMIT, MAX_LOG_LIMIT);
    let walk = PathHistory::new(repo, &tip, path, args.follow_renames, None)?;
    let steps = watch.track(walk, None, "commits found");
    let (commits, truncated) = paginate_built(steps, args.skip.unwrap_or(0), limit, |step| {
        Ok(FileHistoryEntry {
            commit: LogEntry::from_commit(&step.commit)?,
            change: step.change,
            path: step.path.to_string(),
            old_path: step.old_path.map(|p| p.to_string()),
        })
    })?;
    Ok(FileHistoryResult { commits, truncated })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCommitsArgs {
    /// Regular expression matched anywhere in the full commit message (Rust `regex` syntax,
    /// case-sensitive; start with `(?i)` to ignore case).
    pub message_regex: Option<String>,
    /// Regular expression matched against the author as `Name <email>`, like `git log --author`.
    pub author: Option<String>,
    /// Only commits whose committer date is at or after this time. Accepts ISO 8601 (`2024-01-31`,
    /// `2024-01-31T12:00:00Z`), RFC 2822, Unix seconds, or relative dates (`2 weeks ago`, `yesterday`).
    pub since: Option<String>,
    /// Only commits whose committer date is at or before this time. Same formats as `since`.
    pub until: Option<String>,
    /// Only commits that changed this file or directory (relative to the repository root).
    pub path: Option<String>,
    /// Revision to search from (default `HEAD`).
    pub rev: Option<String>,
    /// Maximum commits to return (default 20, max 500).
    pub limit: Option<usize>,
    /// Number of matching commits to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchCommitsResult {
    /// Matching commits, newest first by committer date.
    pub commits: Vec<LogEntry>,
    /// True if more matching commits exist past this page.
    pub truncated: bool,
}

/// Commits reachable from `rev` that match every given filter. Reports commits scanned to `watch` and
/// stops when it's cancelled.
pub fn search_commits(
    repo: &gix::Repository,
    args: SearchCommitsArgs,
    watch: &Watch,
) -> anyhow::Result<SearchCommitsResult> {
    let filter = CommitFilter::new(&args)?;
    let tip = resolve_commit(repo, args.rev.as_deref())?;
    let limit = page_limit(args.limit, DEFAULT_LOG_LIMIT, MAX_LOG_LIMIT);
    let path = args.path.as_deref().map(repo_path).transpose()?;

    let candidates: Box<dyn Iterator<Item = anyhow::Result<gix::Commit<'_>>>> = match path {
        Some(path) => Box::new(
            PathHistory::new(repo, &tip, path, false, filter.since)?.map(|step| Ok(step?.commit)),
        ),
        None => {
            let mut walk = repo
                .rev_walk([tip.id])
                .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst));
            if let Some(since) = filter.since {
                // Stop walking once commits get older than `since` instead of scanning all history.
                walk = walk.sorting(Sorting::ByCommitTimeCutoff {
                    order: CommitTimeOrder::NewestFirst,
                    seconds: since,
                });
            }
            Box::new(walk.all()?.map(|info| Ok(info?.object()?)))
        }
    };
    let scanned = watch.track(candidates, None, "commits scanned");
    let matching = scanned.filter_map(|commit| {
        let step = || -> anyhow::Result<Option<gix::Commit<'_>>> {
            let commit = commit?;
            Ok(filter.matches(&commit)?.then_some(commit))
        };
        step().transpose()
    });
    let (commits, truncated) = paginate_built(matching, args.skip.unwrap_or(0), limit, |commit| {
        LogEntry::from_commit(&commit)
    })?;
    Ok(SearchCommitsResult { commits, truncated })
}

struct CommitFilter {
    message: Option<regex::Regex>,
    author: Option<regex::Regex>,
    since: Option<SecondsSinceUnixEpoch>,
    until: Option<SecondsSinceUnixEpoch>,
}

impl CommitFilter {
    fn new(args: &SearchCommitsArgs) -> anyhow::Result<Self> {
        Ok(Self {
            message: compile_regex("message_regex", args.message_regex.as_deref())?,
            author: compile_regex("author", args.author.as_deref())?,
            since: parse_date("since", args.since.as_deref())?,
            until: parse_date("until", args.until.as_deref())?,
        })
    }

    fn matches(&self, commit: &gix::Commit<'_>) -> anyhow::Result<bool> {
        let time = commit.time()?.seconds;
        if self.since.is_some_and(|since| time < since)
            || self.until.is_some_and(|until| time > until)
        {
            return Ok(false);
        }
        if let Some(re) = &self.message
            && !re.is_match(&commit.message_raw()?.to_str_lossy())
        {
            return Ok(false);
        }
        if let Some(re) = &self.author {
            let author = commit.author()?;
            if !re.is_match(&format!("{} <{}>", author.name, author.email)) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn compile_regex(field: &str, pattern: Option<&str>) -> anyhow::Result<Option<regex::Regex>> {
    pattern
        .map(|p| {
            regex::Regex::new(p)
                .with_context(|| format!("`{field}` is not a valid regular expression"))
        })
        .transpose()
}

pub(crate) fn parse_date(
    field: &str,
    input: Option<&str>,
) -> anyhow::Result<Option<SecondsSinceUnixEpoch>> {
    input
        .map(|s| {
            gix::date::parse(s, Some(gix::date::Zoned::now()))
                .map(|t| t.seconds)
                .with_context(|| {
                    format!("`{field}` value `{s}` is not a date; use e.g. `2024-01-31`, `2024-01-31T12:00:00Z` or `2 weeks ago`")
                })
        })
        .transpose()
}

/// One commit that changed the walked path.
struct PathStep<'repo> {
    commit: gix::Commit<'repo>,
    change: ChangeKind,
    /// The path in this commit.
    path: BString,
    /// The path before this commit, when a followed rename happened here.
    old_path: Option<BString>,
}

/// The history of one path in `git log -- <path>` order: newest committer date first, listing the
/// commits that differ from every parent. A merge that took the path unchanged from one parent is
/// hidden and only that parent is followed: git's default history simplification.
struct PathHistory<'repo> {
    repo: &'repo gix::Repository,
    path: BString,
    follow_renames: bool,
    /// Commits older than this are neither listed nor walked through.
    since: Option<SecondsSinceUnixEpoch>,
    /// Max-heap on commit time. The reversed counter pops equal times first-in-first-out, as git does.
    queue: BinaryHeap<(SecondsSinceUnixEpoch, Reverse<u64>, ObjectId)>,
    pushed: u64,
    seen: HashSet<ObjectId>,
}

impl<'repo> PathHistory<'repo> {
    fn new(
        repo: &'repo gix::Repository,
        tip: &gix::Commit<'repo>,
        path: BString,
        follow_renames: bool,
        since: Option<SecondsSinceUnixEpoch>,
    ) -> anyhow::Result<Self> {
        let mut walk = Self {
            repo,
            path,
            follow_renames,
            since,
            queue: BinaryHeap::new(),
            pushed: 0,
            seen: HashSet::new(),
        };
        walk.push(tip)?;
        Ok(walk)
    }

    fn push(&mut self, commit: &gix::Commit<'_>) -> anyhow::Result<()> {
        let time = commit.time()?.seconds;
        if self.since.is_some_and(|since| time < since) || !self.seen.insert(commit.id) {
            return Ok(());
        }
        self.pushed += 1;
        self.queue.push((time, Reverse(self.pushed), commit.id));
        Ok(())
    }

    /// Queue the parents worth following and report the commit if it changed the path.
    fn visit(&mut self, id: ObjectId) -> anyhow::Result<Option<PathStep<'repo>>> {
        let commit = self.repo.find_commit(id)?;
        let ours = entry_at(&commit, self.path.as_ref())?;
        let mut parents = Vec::new();
        for parent_id in commit.parent_ids() {
            let parent = parent_id.object()?.into_commit();
            let theirs = entry_at(&parent, self.path.as_ref())?;
            if same_entry(ours.as_ref(), theirs.as_ref()) {
                // TREESAME: whatever this commit has at `path` came from this parent alone.
                self.push(&parent)?;
                return Ok(None);
            }
            parents.push((parent, theirs));
        }
        let change = match (parents.first(), &ours) {
            (None, None) => return Ok(None),
            (None, Some(_)) | (Some((_, None)), _) => ChangeKind::Added,
            (Some((_, Some(_))), None) => ChangeKind::Deleted,
            (Some((_, Some(old))), Some(new)) => {
                ChangeKind::of_modification(old.mode.kind(), new.mode.kind())
            }
        };
        for (parent, _) in &parents {
            self.push(parent)?;
        }

        let mut step = PathStep {
            commit,
            change,
            path: self.path.clone(),
            old_path: None,
        };
        if self.follow_renames
            && change == ChangeKind::Added
            && let Some(source) = rename_source(self.repo, &step.commit, self.path.as_ref())?
        {
            step.change = ChangeKind::Renamed;
            step.old_path = Some(source.clone());
            self.path = source;
        }
        Ok(Some(step))
    }
}

impl<'repo> Iterator for PathHistory<'repo> {
    type Item = anyhow::Result<PathStep<'repo>>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((_, _, id)) = self.queue.pop() {
            if let Some(step) = self.visit(id).transpose() {
                return Some(step);
            }
        }
        None
    }
}

/// The tree entry at `path` in `commit`, if any.
fn entry_at(
    commit: &gix::Commit<'_>,
    path: &BStr,
) -> anyhow::Result<Option<gix::objs::tree::Entry>> {
    let tree = commit.tree()?;
    let entry = tree.lookup_entry(path.split(|b| *b == b'/'))?;
    Ok(entry.map(|e| e.detach()))
}

fn same_entry(a: Option<&gix::objs::tree::Entry>, b: Option<&gix::objs::tree::Entry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.oid == b.oid && a.mode == b.mode,
        _ => false,
    }
}

/// If `path` was added in `commit` by renaming another file of its first parent, the old path.
fn rename_source(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    path: &BStr,
) -> anyhow::Result<Option<BString>> {
    let Some(parent) = commit.parent_ids().next() else {
        return Ok(None);
    };
    let old_tree = parent.object()?.into_commit().tree()?;
    let new_tree = commit.tree()?;
    let changes = repo.diff_tree_to_tree(&old_tree, &new_tree, None)?;
    Ok(changes.into_iter().find_map(|change| match change {
        gix::object::tree::diff::ChangeDetached::Rewrite {
            source_location,
            location,
            copy: false,
            ..
        } if location == path => Some(source_location),
        _ => None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        empty_repo, fixture_repo, git_stdout, open, run_git, run_git_with_env, write,
    };

    fn log_args(limit: Option<usize>, skip: Option<usize>) -> LogArgs {
        LogArgs {
            rev: None,
            limit,
            skip,
            first_parent: false,
        }
    }

    fn history_args(path: &str) -> FileHistoryArgs {
        FileHistoryArgs {
            path: path.into(),
            rev: None,
            limit: None,
            skip: None,
            follow_renames: false,
        }
    }

    fn search_args() -> SearchCommitsArgs {
        SearchCommitsArgs {
            message_regex: None,
            author: None,
            since: None,
            until: None,
            path: None,
            rev: None,
            limit: None,
            skip: None,
        }
    }

    fn ids<'a>(entries: impl IntoIterator<Item = &'a LogEntry>) -> Vec<String> {
        entries.into_iter().map(|c| c.id.clone()).collect()
    }

    fn git_ids(dir: &std::path::Path, args: &[&str]) -> Vec<String> {
        let mut cmd = vec!["log", "--format=%H"];
        cmd.extend_from_slice(args);
        git_stdout(dir, &cmd).lines().map(str::to_owned).collect()
    }

    /// Linear history touching several files, then a merge, so path filters have something to skip.
    fn history_fixture() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let git = |args: &[&str]| run_git(root, args);
        write(root, "src/lib.rs", "fn a() {}\n");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add lib"]);
        write(root, "docs/readme.md", "docs\n");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add docs"]);
        git(&["checkout", "-q", "feature"]);
        write(root, "src/feature.rs", "fn f() {}\n");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "feature work\n\nFixes #42"]);
        git(&["checkout", "-q", "main"]);
        write(root, "src/lib.rs", "fn a() {}\nfn b() {}\n");
        git(&["commit", "-q", "-am", "extend lib"]);
        git(&["merge", "-q", "--no-edit", "feature"]);
        let repo = open(root);
        (dir, repo)
    }

    #[test]
    fn log_is_newest_first() {
        let (_dir, repo) = fixture_repo();
        let res = log(&repo, log_args(None, None), &Watch::none()).unwrap();
        let summaries: Vec<_> = res.commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(summaries, ["second commit", "first commit"]);
        assert!(!res.truncated);
        assert_eq!(res.commits[0].author.email, "test@example.com");
    }

    #[test]
    fn log_paginates() {
        let (_dir, repo) = fixture_repo();
        let page = log(&repo, log_args(Some(1), None), &Watch::none()).unwrap();
        assert_eq!(page.commits.len(), 1);
        assert!(page.truncated);
        let next = log(&repo, log_args(Some(1), Some(1)), &Watch::none()).unwrap();
        assert_eq!(next.commits[0].summary, "first commit");
        assert!(!next.truncated);
    }

    #[test]
    fn log_first_parent_skips_merged_side() {
        let (dir, repo) = history_fixture();
        let mut args = log_args(None, None);
        args.first_parent = true;
        let res = log(&repo, args, &Watch::none()).unwrap();
        assert_eq!(
            ids(&res.commits),
            git_ids(dir.path(), &["--first-parent", "--topo-order"])
        );
    }

    #[test]
    fn log_on_empty_repo_explains() {
        let (_dir, repo) = empty_repo();
        let err = format!(
            "{:#}",
            log(&repo, log_args(None, None), &Watch::none()).unwrap_err()
        );
        assert!(err.contains("no commits yet"), "{err}");
    }

    #[test]
    fn show_commit_resolves_revspecs() {
        let (_dir, repo) = fixture_repo();
        let c = show_commit(
            &repo,
            ShowCommitArgs {
                rev: Some("v1~1".into()),
            },
        )
        .unwrap();
        assert_eq!(c.summary, "first commit");
        assert!(c.parents.is_empty());
        let head = show_commit(&repo, ShowCommitArgs { rev: None }).unwrap();
        assert!(head.message.contains("with a body"));
    }

    #[test]
    fn bad_revision_is_an_error() {
        let (_dir, repo) = fixture_repo();
        assert!(
            show_commit(
                &repo,
                ShowCommitArgs {
                    rev: Some("nope".into())
                }
            )
            .is_err()
        );
    }

    #[test]
    fn file_history_matches_git_log_for_files_and_dirs() {
        let (dir, repo) = history_fixture();
        for path in [
            "a.txt",
            "src/lib.rs",
            "src",
            "docs/readme.md",
            "src/feature.rs",
        ] {
            let res = file_history(&repo, history_args(path), &Watch::none()).unwrap();
            let ours: Vec<_> = res.commits.iter().map(|c| c.commit.id.clone()).collect();
            assert_eq!(ours, git_ids(dir.path(), &["--", path]), "{path}");
        }
    }

    #[test]
    fn file_history_reports_change_kinds() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "tmp.txt", "x\n");
        run_git(root, &["add", "tmp.txt"]);
        run_git(root, &["commit", "-q", "-m", "add tmp"]);
        run_git(root, &["rm", "-q", "tmp.txt"]);
        run_git(root, &["commit", "-q", "-m", "drop tmp"]);
        let res = file_history(&open(root), history_args("tmp.txt"), &Watch::none()).unwrap();
        let changes: Vec<_> = res.commits.iter().map(|c| c.change).collect();
        assert_eq!(changes, [ChangeKind::Deleted, ChangeKind::Added]);
        let res = file_history(&open(root), history_args("a.txt"), &Watch::none()).unwrap();
        let changes: Vec<_> = res.commits.iter().map(|c| c.change).collect();
        assert_eq!(changes, [ChangeKind::Modified, ChangeKind::Added]);
    }

    #[test]
    fn file_history_follows_renames_like_git() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["mv", "a.txt", "moved.txt"]);
        run_git(root, &["commit", "-q", "-m", "move a"]);
        write(root, "moved.txt", "one\ntwo\nthree\n");
        run_git(root, &["commit", "-q", "-am", "edit moved"]);

        let repo = open(root);
        let plain = file_history(&repo, history_args("moved.txt"), &Watch::none()).unwrap();
        assert_eq!(
            plain.commits.len(),
            2,
            "without follow, history starts at the rename"
        );
        assert_eq!(plain.commits[1].change, ChangeKind::Added);

        let mut args = history_args("moved.txt");
        args.follow_renames = true;
        let res = file_history(&repo, args, &Watch::none()).unwrap();
        let ours: Vec<_> = res.commits.iter().map(|c| c.commit.id.clone()).collect();
        assert_eq!(ours, git_ids(root, &["--follow", "--", "moved.txt"]));
        let rename = &res.commits[1];
        assert_eq!(rename.change, ChangeKind::Renamed);
        assert_eq!(rename.old_path.as_deref(), Some("a.txt"));
        assert_eq!(res.commits[2].path, "a.txt");
    }

    #[test]
    fn file_history_paginates_and_validates() {
        let (dir, repo) = history_fixture();
        let all = git_ids(dir.path(), &["--", "src"]);
        assert_eq!(
            all.len(),
            4,
            "the merge touches src from both sides, so it is listed"
        );
        let mut args = history_args("src");
        args.limit = Some(1);
        let first = file_history(&repo, args, &Watch::none()).unwrap();
        assert!(first.truncated);
        assert_eq!(first.commits[0].commit.id, all[0]);
        let mut args = history_args("src");
        args.skip = Some(1);
        let rest = file_history(&repo, args, &Watch::none()).unwrap();
        let rest: Vec<_> = rest.commits.iter().map(|c| c.commit.id.clone()).collect();
        assert_eq!(rest, all[1..]);

        let mut args = history_args("src");
        args.follow_renames = true;
        assert!(
            file_history(&repo, args, &Watch::none()).is_err(),
            "follow needs a file"
        );
        assert!(file_history(&repo, history_args("../x"), &Watch::none()).is_err());
        assert!(
            file_history(&repo, history_args("missing"), &Watch::none())
                .unwrap()
                .commits
                .is_empty()
        );
    }

    #[test]
    fn search_by_message_author_and_path() {
        let (dir, repo) = history_fixture();
        let root = dir.path();
        let mut args = search_args();
        args.message_regex = Some("(?i)FIXES #\\d+".into());
        let res = search_commits(&repo, args, &Watch::none()).unwrap();
        assert_eq!(
            ids(&res.commits),
            git_ids(root, &["-i", "--grep=fixes #[0-9]"])
        );
        assert_eq!(res.commits.len(), 1);

        let mut args = search_args();
        args.author = Some("test@example\\.com".into());
        let res = search_commits(&repo, args, &Watch::none()).unwrap();
        assert_eq!(res.commits.len(), git_ids(root, &[]).len());

        let mut args = search_args();
        args.author = Some("^Nobody".into());
        assert!(
            search_commits(&repo, args, &Watch::none())
                .unwrap()
                .commits
                .is_empty()
        );

        let mut args = search_args();
        args.path = Some("src/lib.rs".into());
        args.message_regex = Some("lib".into());
        let res = search_commits(&repo, args, &Watch::none()).unwrap();
        assert_eq!(
            ids(&res.commits),
            git_ids(root, &["--grep=lib", "--", "src/lib.rs"])
        );
    }

    #[test]
    fn search_by_date_window() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        for (msg, date) in [
            ("old", "2020-01-01T00:00:00Z"),
            ("mid", "2022-06-01T00:00:00Z"),
            ("new", "2024-01-01T00:00:00Z"),
        ] {
            write(root, "dated.txt", msg);
            run_git(root, &["add", "dated.txt"]);
            run_git_with_env(
                root,
                &["commit", "-q", "-m", msg],
                &[("GIT_COMMITTER_DATE", date), ("GIT_AUTHOR_DATE", date)],
            );
        }
        let repo = open(root);
        let mut args = search_args();
        args.since = Some("2021-01-01".into());
        args.until = Some("2023-01-01".into());
        let res = search_commits(&repo, args, &Watch::none()).unwrap();
        assert_eq!(
            ids(&res.commits),
            git_ids(root, &["--since=2021-01-01", "--until=2023-01-01"])
        );
        assert_eq!(res.commits[0].summary, "mid");

        let mut args = search_args();
        args.since = Some("2999-01-01".into());
        assert!(
            search_commits(&repo, args, &Watch::none())
                .unwrap()
                .commits
                .is_empty()
        );
        let mut args = search_args();
        args.since = Some("10 years ago".into());
        assert_eq!(
            search_commits(&repo, args, &Watch::none())
                .unwrap()
                .commits
                .len(),
            5
        );
    }

    #[test]
    fn search_rejects_bad_input() {
        let (_dir, repo) = fixture_repo();
        let mut args = search_args();
        args.message_regex = Some("(".into());
        let err = format!(
            "{:#}",
            search_commits(&repo, args, &Watch::none()).unwrap_err()
        );
        assert!(err.contains("message_regex"), "{err}");

        let mut args = search_args();
        args.since = Some("not a date at all".into());
        let err = format!(
            "{:#}",
            search_commits(&repo, args, &Watch::none()).unwrap_err()
        );
        assert!(err.contains("`since`"), "{err}");
    }

    #[test]
    fn search_paginates() {
        let (_dir, repo) = history_fixture();
        let mut args = search_args();
        args.limit = Some(2);
        let page = search_commits(&repo, args, &Watch::none()).unwrap();
        assert_eq!(page.commits.len(), 2);
        assert!(page.truncated);
    }

    #[test]
    fn walks_report_progress_and_stop_when_cancelled() {
        use crate::tools::testutil::{cancelled_watch, recording_watch};
        let (dir, repo) = fixture_repo();
        let commits = git_stdout(dir.path(), &["rev-list", "--count", "HEAD"]);
        let commits: u64 = commits.trim().parse().unwrap();

        let (watch, _, seen) = recording_watch(std::time::Duration::ZERO);
        log(&repo, log_args(Some(500), None), &watch).unwrap();
        let last = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!((last.done, last.total), (commits, None));
        assert_eq!(last.message, format!("{commits} commits walked"));

        let (watch, _, seen) = recording_watch(std::time::Duration::ZERO);
        search_commits(&repo, search_args(), &watch).unwrap();
        assert!(
            seen.lock()
                .unwrap()
                .last()
                .unwrap()
                .message
                .ends_with("commits scanned")
        );

        let (watch, _, seen) = recording_watch(std::time::Duration::ZERO);
        file_history(&repo, history_args("a.txt"), &watch).unwrap();
        assert!(
            seen.lock()
                .unwrap()
                .last()
                .unwrap()
                .message
                .ends_with("commits found")
        );

        let cancelled = [
            log(&repo, log_args(None, None), &cancelled_watch()).map(drop),
            search_commits(&repo, search_args(), &cancelled_watch()).map(drop),
            file_history(&repo, history_args("a.txt"), &cancelled_watch()).map(drop),
        ];
        for result in cancelled {
            assert!(result.unwrap_err().to_string().contains("cancelled"));
        }
    }

    proptest::proptest! {
        /// Dates are user text: parsing never panics.
        #[test]
        fn any_date_parses_or_errors(input in "\\PC{0,24}") {
            let _ = parse_date("since", Some(&input));
        }
    }

    /// Revisions are user text: resolving never panics, whatever the spec.
    #[test]
    fn any_revision_resolves_or_errors() {
        let (_dir, repo) = fixture_repo();
        let mut runner = proptest::test_runner::TestRunner::default();
        runner
            .run(&"[A-Za-z0-9@{}^~:./!-]{0,16}", |spec| {
                let _ = crate::tools::resolve_object(&repo, Some(&spec));
                Ok(())
            })
            .unwrap();
    }
}
