//! Tool implementations: sync functions over `&gix::Repository` returning
//! serde/schemars types that become the tools' structured output.
//!
//! Shared output types and argument helpers (rev defaults, pagination, byte
//! budgets, path normalization) live here so every tool behaves the same way.

pub mod attributes;
pub mod blame;
pub mod branches;
pub mod checkout;
pub mod clean;
pub mod commit;
pub mod config;
pub mod diff;
pub mod files;
pub mod grep;
pub mod history;
pub mod merge;
pub mod notes;
pub mod objects;
pub mod reflog;
pub mod refs;
pub mod repo;
pub mod revision;
pub mod staging;
pub mod stash;
pub mod status;
pub mod submodules;
pub mod worktrees;
pub mod write;

use anyhow::{Context, bail};
use gix::bstr::BString;
use schemars::JsonSchema;
use serde::Serialize;

/// Default byte budget for content-bearing results (file content, patches): about 64 KiB per call.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;
/// Upper bound for a caller-supplied `max_bytes`.
pub const MAX_MAX_BYTES: usize = 1024 * 1024;
/// Git calls content binary if a NUL byte appears in its first 8000 bytes.
const BINARY_SNIFF_LEN: usize = 8000;

/// A person + timestamp, as recorded in a commit or tag.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Signature {
    /// Name as recorded in the commit.
    pub name: String,
    /// Email as recorded in the commit.
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
    /// UTC offset in seconds.
    pub offset: i32,
}

impl Signature {
    pub fn from_ref(sig: gix::actor::SignatureRef<'_>) -> anyhow::Result<Self> {
        let time = sig.time()?;
        Ok(Self {
            name: sig.name.to_string(),
            email: sig.email.to_string(),
            time: time.seconds,
            offset: time.offset,
        })
    }

    /// From an already parsed signature, such as one in a reflog line.
    pub fn from_owned(sig: &gix::actor::Signature) -> Self {
        Self {
            name: sig.name.to_string(),
            email: sig.email.to_string(),
            time: sig.time.seconds,
            offset: sig.time.offset,
        }
    }
}

/// A git object type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObjectType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl From<gix::object::Kind> for ObjectType {
    fn from(kind: gix::object::Kind) -> Self {
        match kind {
            gix::object::Kind::Commit => Self::Commit,
            gix::object::Kind::Tree => Self::Tree,
            gix::object::Kind::Blob => Self::Blob,
            gix::object::Kind::Tag => Self::Tag,
        }
    }
}

/// One commit in a history listing.
#[derive(Debug, Serialize, JsonSchema)]
pub struct LogEntry {
    /// Full commit SHA.
    pub id: String,
    /// Abbreviated SHA, unique within the repository.
    pub short_id: String,
    /// First line of the commit message.
    pub summary: String,
    /// Who wrote the change, and when.
    pub author: Signature,
    /// Full SHAs of the parent commits; empty for a root commit.
    pub parents: Vec<String>,
}

impl LogEntry {
    pub fn from_commit(commit: &gix::Commit<'_>) -> anyhow::Result<Self> {
        Ok(Self {
            id: commit.id.to_string(),
            short_id: commit.short_id()?.to_string(),
            summary: commit.message()?.summary().to_string(),
            author: Signature::from_ref(commit.author()?)?,
            parents: commit.parent_ids().map(|p| p.to_string()).collect(),
        })
    }
}

/// How a path changed between two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Moved from `old_path`, possibly with edits.
    Renamed,
    /// Copied from `old_path`, possibly with edits.
    Copied,
    /// Changed type, e.g. from a regular file to a symlink.
    Typechange,
}

impl ChangeKind {
    /// `Modified`, or `Typechange` when the entry switched between file, symlink, tree or submodule.
    /// Flipping only the executable bit is a modification, as in git.
    pub fn of_modification(
        old: gix::object::tree::EntryKind,
        new: gix::object::tree::EntryKind,
    ) -> Self {
        use gix::object::tree::EntryKind::{Blob, BlobExecutable};
        let same_type =
            old == new || matches!((old, new), (Blob, BlobExecutable) | (BlobExecutable, Blob));
        if same_type {
            Self::Modified
        } else {
            Self::Typechange
        }
    }
}

/// Which sides of a merge touched a conflicted path, as in `git status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both sides changed the path (`UU`): base, ours and theirs all exist.
    BothModified,
    /// Both sides added the path with different content (`AA`): no base.
    BothAdded,
    /// Both sides deleted or renamed away the path (`DD`): only the base exists.
    BothDeleted,
    /// Only our side added the path (`AU`), e.g. while theirs renamed it elsewhere.
    AddedByUs,
    /// Only their side added the path (`UA`).
    AddedByThem,
    /// We deleted the path and they modified it (`DU`).
    DeletedByUs,
    /// They deleted the path and we modified it (`UD`).
    DeletedByThem,
}

impl From<gix::status::plumbing::index_as_worktree::Conflict> for ConflictKind {
    fn from(conflict: gix::status::plumbing::index_as_worktree::Conflict) -> Self {
        use gix::status::plumbing::index_as_worktree::Conflict;
        match conflict {
            Conflict::BothModified => Self::BothModified,
            Conflict::BothAdded => Self::BothAdded,
            Conflict::BothDeleted => Self::BothDeleted,
            Conflict::AddedByUs => Self::AddedByUs,
            Conflict::AddedByThem => Self::AddedByThem,
            Conflict::DeletedByUs => Self::DeletedByUs,
            Conflict::DeletedByThem => Self::DeletedByThem,
        }
    }
}

impl ConflictKind {
    /// The kind from which index stages exist for a path: base (1), ours (2) and theirs (3).
    /// `None` when no stage exists, so the path isn't conflicted.
    pub fn from_stages(base: bool, ours: bool, theirs: bool) -> Option<Self> {
        Some(match (base, ours, theirs) {
            (true, true, true) => Self::BothModified,
            (false, true, true) => Self::BothAdded,
            (true, false, false) => Self::BothDeleted,
            (false, true, false) => Self::AddedByUs,
            (false, false, true) => Self::AddedByThem,
            (true, false, true) => Self::DeletedByUs,
            (true, true, false) => Self::DeletedByThem,
            (false, false, false) => return None,
        })
    }
}

/// Clamp a caller-supplied page size to `1..=max`, defaulting to `default`.
pub fn page_limit(limit: Option<usize>, default: usize, max: usize) -> usize {
    limit.unwrap_or(default).clamp(1, max)
}

/// Clamp a caller-supplied byte budget to `1..=MAX_MAX_BYTES`, defaulting to [`DEFAULT_MAX_BYTES`].
pub fn byte_budget(max_bytes: Option<usize>) -> usize {
    max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(1, MAX_MAX_BYTES)
}

/// Collect one page from `items`: skip `skip`, take `limit`, and report whether more remained.
///
/// Only pulls one item past the page, so lazy walks stay cheap.
pub fn paginate<T>(
    items: impl IntoIterator<Item = anyhow::Result<T>>,
    skip: usize,
    limit: usize,
) -> anyhow::Result<(Vec<T>, bool)> {
    let mut iter = items.into_iter().skip(skip);
    let page = iter
        .by_ref()
        .take(limit)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let truncated = iter.next().transpose()?.is_some();
    Ok((page, truncated))
}

/// Resolve `rev` (default `HEAD`) to a commit, peeling tags.
pub fn resolve_commit<'repo>(
    repo: &'repo gix::Repository,
    rev: Option<&str>,
) -> anyhow::Result<gix::Commit<'repo>> {
    let rev = rev.unwrap_or("HEAD");
    ensure_head_is_born(repo, rev)?;
    repo.rev_parse_single(rev)
        .map_err(anyhow::Error::from)
        .and_then(|id| Ok(id.object()?.peel_to_commit()?))
        .with_context(|| {
            format!(
                "cannot resolve `{rev}` to a commit; pass a branch, tag, full or abbreviated SHA, \
                 or an expression such as HEAD~2 (list_refs shows what exists)"
            )
        })
}

/// Resolve `rev` (default `HEAD`) to whatever object it names, without peeling tags.
pub fn resolve_object<'repo>(
    repo: &'repo gix::Repository,
    rev: Option<&str>,
) -> anyhow::Result<gix::Id<'repo>> {
    let rev = rev.unwrap_or("HEAD");
    ensure_head_is_born(repo, rev)?;
    repo.rev_parse_single(rev).with_context(|| {
        format!(
            "cannot resolve `{rev}`; pass a branch, tag, full or abbreviated SHA, \
             or an expression such as HEAD~2 (list_refs shows what exists)"
        )
    })
}

/// Resolve `rev` (default `HEAD`) to a tree: commits and tags are peeled.
pub fn resolve_tree<'repo>(
    repo: &'repo gix::Repository,
    rev: Option<&str>,
) -> anyhow::Result<gix::Tree<'repo>> {
    let rev = rev.unwrap_or("HEAD");
    ensure_head_is_born(repo, rev)?;
    repo.rev_parse_single(rev)
        .map_err(anyhow::Error::from)
        .and_then(|id| Ok(id.object()?.peel_to_tree()?))
        .with_context(|| {
            format!(
                "cannot resolve `{rev}` to a tree; pass a commit-ish (branch, tag, SHA, HEAD~2) \
                 or a tree SHA"
            )
        })
}

/// A fresh repository has a HEAD that names a branch with no commits yet; say so plainly
/// instead of surfacing a generic "reference not found".
fn ensure_head_is_born(repo: &gix::Repository, rev: &str) -> anyhow::Result<()> {
    if !matches!(rev, "HEAD" | "@") {
        return Ok(());
    }
    if let gix::head::Kind::Unborn(name) = repo.head()?.kind {
        bail!(
            "HEAD points to `{}`, which has no commits yet; commit something first or pass another revision",
            name.shorten()
        );
    }
    Ok(())
}

/// Normalize a caller-supplied repository-relative path: strips `./`, duplicate and trailing `/`,
/// and rejects absolute paths and `..` so nothing can escape the repository.
///
/// The repository root (`""` or `"."`) normalizes to an empty path.
pub fn repo_path(path: &str) -> anyhow::Result<BString> {
    if path.starts_with('/') || path.starts_with('\\') {
        bail!(
            "path `{path}` is absolute; pass a path relative to the repository root, e.g. `src/main.rs`"
        );
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                bail!("path `{path}` contains `..`; pass a path relative to the repository root")
            }
            part => parts.push(part),
        }
    }
    Ok(parts.join("/").into())
}

/// Like [`repo_path`], but the root itself isn't a valid answer.
pub fn repo_file_path(path: &str) -> anyhow::Result<BString> {
    let normalized = repo_path(path)?;
    if normalized.is_empty() {
        bail!(
            "`path` is empty; pass a file path relative to the repository root, e.g. `src/main.rs`"
        );
    }
    Ok(normalized)
}

/// Caller-supplied pathspecs, passed to gix as-is so magic like `:(exclude)` keeps working.
/// They're relative to the repository root because the server runs from there (see `RepoHandle::discover`).
pub fn pathspec_patterns(pathspec: &[String]) -> Vec<BString> {
    pathspec.iter().map(|p| BString::from(p.as_str())).collect()
}

/// A pathspec search over `pathspec`, or `None` to include everything.
pub fn pathspec_search<'repo>(
    repo: &'repo gix::Repository,
    pathspec: &[String],
    index: &gix::index::State,
) -> anyhow::Result<Option<gix::Pathspec<'repo>>> {
    if pathspec.is_empty() {
        return Ok(None);
    }
    Ok(Some(repo.pathspec(
        false,
        pathspec_patterns(pathspec),
        true,
        index,
        gix::worktree::stack::state::attributes::Source::IdMapping,
    )?))
}

/// Whether `data` is binary by git's rule: a NUL byte in the first 8000 bytes.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF_LEN)].contains(&0)
}

/// The tree-entry kind (file, executable, symlink, submodule) an index entry mode stands for.
pub fn index_entry_kind(mode: gix::index::entry::Mode) -> gix::object::tree::EntryKind {
    mode.to_tree_entry_mode()
        .map_or(gix::object::tree::EntryKind::Blob, |m| m.kind())
}

/// The longest prefix of `text` that fits in `max_bytes` without splitting a character, and whether
/// anything was cut off.
pub fn clip_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// A tree entry kind as git prints its mode: `100644`, `100755`, `120000`, `040000` or `160000`.
pub fn octal_mode(kind: gix::object::tree::EntryKind) -> String {
    format!("{:06o}", kind as u16)
}

/// The operation in progress (`Merge`, `Rebase`, `CherryPick`, ...), if any.
pub fn in_progress(repo: &gix::Repository) -> Option<String> {
    repo.state().map(|state| format!("{state:?}"))
}

/// Format an object id abbreviated to git's default seven hex digits.
pub fn short_hex(id: &gix::oid) -> String {
    id.to_hex_with_len(7).to_string()
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::{path::Path, process::Command};

    /// Build a throwaway repo with the system `git`, which doubles as an oracle
    /// for gix behaving like real git.
    pub fn fixture_repo() -> (tempfile::TempDir, gix::Repository) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| run_git(dir.path(), args);
        init(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "first commit"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(&["commit", "-q", "-am", "second commit\n\nwith a body"]);
        git(&["tag", "v1"]);
        git(&["branch", "feature"]);
        let repo = open(dir.path());
        (dir, repo)
    }

    /// An initialized repository on `main` with no commits.
    pub fn empty_repo() -> (tempfile::TempDir, gix::Repository) {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path());
        let repo = open(dir.path());
        (dir, repo)
    }

    /// `git init` on `main` with an identity in the local config: repositories opened by [`open`]
    /// ignore the environment, and write tools need a committer for commits and reflogs.
    fn init(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.name", "Test"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
    }

    /// Assert that everything below `dir` still matches `before` (from [`snapshot`]), naming the
    /// paths that were added, removed or changed rather than dumping their bytes.
    pub fn assert_unchanged(
        dir: &Path,
        before: &std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        let after = snapshot(dir);
        let changed: Vec<_> = before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert!(changed.is_empty(), "these files changed: {changed:#?}");
    }

    /// The working tree's files below `root` (outside `.git`), by relative path, with their content
    /// (symlinks by target) and whether they're executable, to compare two checkouts.
    pub fn worktree_files(
        root: &Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, (Vec<u8>, bool)> {
        use std::os::unix::fs::PermissionsExt;
        snapshot(root)
            .into_iter()
            .filter_map(|(path, content)| {
                let rela = path.strip_prefix(root).ok()?.to_owned();
                if rela.starts_with(".git") {
                    return None;
                }
                let meta = std::fs::symlink_metadata(&path).ok()?;
                let executable = !meta.is_symlink() && meta.permissions().mode() & 0o111 != 0;
                Some((rela, (content, executable)))
            })
            .collect()
    }

    /// Two identical fixture repositories after `setup`, so one can be changed with gitrs and the
    /// other with the git command it mirrors, then compared with [`ls_files`] and [`porcelain`].
    pub fn twins(setup: impl Fn(&Path)) -> [(tempfile::TempDir, gix::Repository); 2] {
        [0, 1].map(|_| {
            let (dir, _) = fixture_repo();
            setup(dir.path());
            let repo = open(dir.path());
            (dir, repo)
        })
    }

    /// `git ls-files -s`: mode, blob, stage and path of every index entry.
    pub fn ls_files(root: &Path) -> String {
        git_stdout(root, &["-c", "core.quotePath=false", "ls-files", "-s"])
    }

    /// `git status --porcelain=v2`: how git itself sees the index and working tree.
    pub fn porcelain(root: &Path) -> String {
        git_stdout(
            root,
            &["-c", "core.quotePath=false", "status", "--porcelain=v2"],
        )
    }

    /// Every file below `dir` with its content, to prove an operation changed nothing on disk.
    pub fn snapshot(dir: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    walk(&path, out);
                } else if meta.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    out.insert(path, target.display().to_string().into_bytes());
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(dir, &mut out);
        out
    }

    /// A bare clone of [`fixture_repo`].
    pub fn bare_repo() -> (tempfile::TempDir, gix::Repository) {
        let (src, _) = fixture_repo();
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare.git");
        run_git(
            dir.path(),
            &[
                "clone",
                "-q",
                "--bare",
                src.path().to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        // A clone doesn't copy the identity, which write tools need.
        run_git(&bare, &["config", "user.name", "Test"]);
        run_git(&bare, &["config", "user.email", "test@example.com"]);
        let repo = open(&bare);
        (dir, repo)
    }

    /// Open with global and system git config ignored, matching the environment `run_git` gives the oracle.
    pub fn open(path: &Path) -> gix::Repository {
        gix::open_opts(path, gix::open::Options::isolated()).unwrap()
    }

    /// Write `content` to `rel` under `root`, creating parent directories.
    pub fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Run git and report whether it succeeded, for commands expected to fail (e.g. a conflicting merge).
    pub fn try_git(cwd: &Path, args: &[&str]) -> bool {
        git_command(cwd, args).output().unwrap().status.success()
    }

    pub fn run_git(cwd: &Path, args: &[&str]) {
        run_git_with_env(cwd, args, &[]);
    }

    /// Like [`run_git`] with extra environment, e.g. `GIT_COMMITTER_DATE` to pin commit times.
    pub fn run_git_with_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) {
        let out = git_command(cwd, args)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Run git and return its stdout: the oracle side of a comparison.
    pub fn git_stdout(cwd: &Path, args: &[&str]) -> String {
        let out = git_command(cwd, args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Run git with `input` on stdin and return its output without checking the exit status, for
    /// oracles such as `git grep` and `git check-ignore` that exit 1 when nothing matches.
    pub fn git_output(cwd: &Path, args: &[&str], input: &[u8]) -> std::process::Output {
        use std::{io::Write as _, process::Stdio};
        let mut child = git_command(cwd, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn git_command(cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{empty_repo, fixture_repo};

    #[test]
    fn repo_path_normalizes_and_rejects_escapes() {
        assert_eq!(repo_path("./src//main.rs/").unwrap(), "src/main.rs");
        assert_eq!(repo_path("").unwrap(), "");
        assert_eq!(repo_path(".").unwrap(), "");
        assert!(repo_path("/etc/passwd").is_err());
        assert!(repo_path("src/../../x").is_err());
        assert!(repo_file_path("./").is_err());
    }

    #[test]
    fn page_limit_and_budget_clamp() {
        assert_eq!(page_limit(None, 20, 500), 20);
        assert_eq!(page_limit(Some(0), 20, 500), 1);
        assert_eq!(page_limit(Some(9999), 20, 500), 500);
        assert_eq!(byte_budget(None), DEFAULT_MAX_BYTES);
        assert_eq!(byte_budget(Some(usize::MAX)), MAX_MAX_BYTES);
    }

    #[test]
    fn paginate_reports_truncation_at_boundaries() {
        let items = || (0..5).map(anyhow::Ok);
        assert_eq!(
            paginate(items(), 0, 5).unwrap(),
            (vec![0, 1, 2, 3, 4], false)
        );
        assert_eq!(paginate(items(), 0, 4).unwrap(), (vec![0, 1, 2, 3], true));
        assert_eq!(paginate(items(), 4, 4).unwrap(), (vec![4], false));
        assert_eq!(paginate(items(), 9, 4).unwrap(), (vec![], false));
        let failing = [Ok(1), Err(anyhow::anyhow!("boom"))];
        assert!(
            paginate(failing, 0, 1).is_err(),
            "errors just past the page surface"
        );
    }

    #[test]
    fn resolve_commit_explains_failures() {
        let (_dir, repo) = fixture_repo();
        assert_eq!(
            resolve_commit(&repo, Some("v1")).unwrap().id,
            repo.head_id().unwrap()
        );
        let err = format!("{:#}", resolve_commit(&repo, Some("nope")).unwrap_err());
        assert!(err.contains("cannot resolve `nope`"), "{err}");

        let (_dir, empty) = empty_repo();
        let err = format!("{:#}", resolve_commit(&empty, None).unwrap_err());
        assert!(err.contains("no commits yet"), "{err}");
        assert!(resolve_tree(&empty, None).is_err());
    }

    #[test]
    fn clip_utf8_keeps_characters_whole() {
        assert_eq!(clip_utf8("abc", 3), ("abc", false));
        assert_eq!(clip_utf8("abc", 2), ("ab", true));
        assert_eq!(clip_utf8("aé", 2), ("a", true));
        assert_eq!(clip_utf8("é", 1), ("", true));
    }

    #[test]
    fn resolve_object_keeps_tags() {
        let (dir, _) = fixture_repo();
        crate::tools::testutil::run_git(dir.path(), &["tag", "-a", "v2", "-m", "release"]);
        let repo = crate::tools::testutil::open(dir.path());
        let tag = resolve_object(&repo, Some("v2")).unwrap();
        assert_eq!(tag.object().unwrap().kind, gix::object::Kind::Tag);
        assert!(resolve_object(&repo, Some("nope")).is_err());
    }

    #[test]
    fn typechange_ignores_executable_bit() {
        use gix::object::tree::EntryKind::*;
        assert_eq!(
            ChangeKind::of_modification(Blob, BlobExecutable),
            ChangeKind::Modified
        );
        assert_eq!(
            ChangeKind::of_modification(Blob, Link),
            ChangeKind::Typechange
        );
    }
}
