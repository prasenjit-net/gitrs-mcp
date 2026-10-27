use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine as _;
use gix::{bstr::BStr, index::entry::Stage, object::tree::EntryKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    byte_budget, index_entry_kind, is_binary, page_limit, repo_file_path, repo_path, resolve_tree,
};

const DEFAULT_TREE_LIMIT: usize = 500;
const MAX_TREE_LIMIT: usize = 5000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// File path relative to the repository root, e.g. `src/main.rs`.
    pub path: String,
    /// Where to read from: a revision (default `HEAD`; any branch, tag, SHA or expression like `HEAD~2`),
    /// `index` for the staged version, or `worktree` for the file on disk. A branch literally named
    /// `index` or `worktree` can be read as `refs/heads/index`.
    pub rev: Option<String>,
    /// Byte offset to start reading at (default 0). Pass `next_offset` from a truncated result to continue.
    pub offset_bytes: Option<u64>,
    /// Maximum bytes of content to return (default 65536, max 1048576).
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadFileResult {
    /// The normalized path that was read.
    pub path: String,
    /// The requested range as UTF-8 text; `null` when `binary` is true.
    pub content: Option<String>,
    /// The requested range as standard base64; `null` unless `binary` is true.
    pub base64: Option<String>,
    /// True when the file contains a NUL byte in its first 8000 bytes (git's rule) or the range isn't valid UTF-8.
    pub binary: bool,
    /// Total size of the file in bytes.
    pub size: u64,
    /// Git file mode in octal: `100644` regular, `100755` executable, `120000` symlink (content is the link target).
    pub mode: String,
    /// SHA of the blob; `null` when reading from the worktree.
    pub blob_id: Option<String>,
    /// True if the returned range ends before the end of the file.
    pub truncated: bool,
    /// Offset to pass as `offset_bytes` to read the next chunk; `null` once the end is reached.
    pub next_offset: Option<u64>,
}

/// File content and metadata, before the requested range is cut out.
struct FileData {
    data: Vec<u8>,
    kind: EntryKind,
    blob_id: Option<gix::ObjectId>,
}

pub fn read_file(repo: &gix::Repository, args: ReadFileArgs) -> anyhow::Result<ReadFileResult> {
    let path = repo_file_path(&args.path)?;
    let file = match args.rev.as_deref() {
        Some("index") => read_from_index(repo, path.as_ref())?,
        Some("worktree") => read_from_worktree(repo, path.as_ref())?,
        rev => read_from_tree(repo, rev, path.as_ref())?,
    };

    let size = file.data.len();
    let offset = usize::try_from(args.offset_bytes.unwrap_or(0)).unwrap_or(usize::MAX);
    if offset > size {
        bail!(
            "offset_bytes {offset} is past the end of `{path}` ({size} bytes); pass a smaller offset"
        );
    }
    let end = size.min(offset.saturating_add(byte_budget(args.max_bytes)));
    let (content, base64, end) = match text_range(&file.data[offset..end], end < size) {
        Some(text) if !is_binary(&file.data) => (Some(text.to_owned()), None, offset + text.len()),
        _ => (
            None,
            Some(base64::engine::general_purpose::STANDARD.encode(&file.data[offset..end])),
            end,
        ),
    };
    Ok(ReadFileResult {
        path: path.to_string(),
        binary: content.is_none(),
        content,
        base64,
        size: size as u64,
        mode: octal_mode(file.kind),
        blob_id: file.blob_id.map(|id| id.to_string()),
        truncated: end < size,
        next_offset: (end < size).then_some(end as u64),
    })
}

/// The range as text, dropping a multi-byte character split by the byte budget so the next chunk
/// starts on a character boundary. `None` if it isn't UTF-8.
fn text_range(bytes: &[u8], more_follows: bool) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(err) if more_follows && err.error_len().is_none() && err.valid_up_to() > 0 => {
            std::str::from_utf8(&bytes[..err.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

fn read_from_tree(
    repo: &gix::Repository,
    rev: Option<&str>,
    path: &BStr,
) -> anyhow::Result<FileData> {
    let rev_name = rev.unwrap_or("HEAD");
    let tree = resolve_tree(repo, rev)?;
    let entry = tree
        .lookup_entry(path.split(|b| *b == b'/'))?
        .with_context(|| {
            format!("`{path}` does not exist at `{rev_name}`; check the path with list_tree")
        })?;
    let kind = entry.mode().kind();
    match kind {
        EntryKind::Tree => {
            bail!("`{path}` is a directory at `{rev_name}`; use list_tree to see its contents")
        }
        EntryKind::Commit => {
            bail!("`{path}` is a submodule at `{rev_name}`; its files live in another repository")
        }
        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {}
    }
    let blob = entry.object()?.into_blob();
    Ok(FileData {
        blob_id: Some(blob.id),
        data: blob.detach().data,
        kind,
    })
}

fn read_from_index(repo: &gix::Repository, path: &BStr) -> anyhow::Result<FileData> {
    let index = repo.index_or_empty()?;
    let Some(entry) = index.entry_by_path_and_stage(path, Stage::Unconflicted) else {
        if index
            .entry_index_by_path_and_stage(path, Stage::Ours)
            .is_some()
            || index
                .entry_index_by_path_and_stage(path, Stage::Theirs)
                .is_some()
        {
            bail!(
                "`{path}` has unresolved merge conflicts in the index; read it from `worktree` to see the conflict markers"
            );
        }
        bail!(
            "`{path}` is not in the index; it may be untracked (read it from `worktree`) or deleted"
        );
    };
    let kind = index_entry_kind(entry.mode);
    if kind == EntryKind::Commit {
        bail!("`{path}` is a submodule; its files live in another repository");
    }
    let blob = repo.find_blob(entry.id)?;
    Ok(FileData {
        blob_id: Some(blob.id),
        data: blob.detach().data,
        kind,
    })
}

fn read_from_worktree(repo: &gix::Repository, path: &BStr) -> anyhow::Result<FileData> {
    let workdir = repo
        .workdir()
        .context("this repository is bare and has no worktree; read from a revision instead")?;
    let full = worktree_path(workdir, path)?;
    let meta = std::fs::symlink_metadata(&full)
        .with_context(|| format!("`{path}` does not exist in the worktree"))?;
    let (data, kind) = if meta.is_symlink() {
        // Like git, report a symlink by its target rather than following it (possibly out of the repo).
        let target = std::fs::read_link(&full)?;
        (
            gix::path::into_bstr(target)?.into_owned().into(),
            EntryKind::Link,
        )
    } else if meta.is_dir() {
        bail!("`{path}` is a directory; use list_tree, or status for untracked files")
    } else {
        (std::fs::read(&full)?, worktree_file_kind(&meta))
    };
    Ok(FileData {
        data,
        kind,
        blob_id: None,
    })
}

#[cfg(unix)]
fn worktree_file_kind(meta: &std::fs::Metadata) -> EntryKind {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        EntryKind::BlobExecutable
    } else {
        EntryKind::Blob
    }
}

#[cfg(not(unix))]
fn worktree_file_kind(_meta: &std::fs::Metadata) -> EntryKind {
    EntryKind::Blob
}

/// `path` under the worktree, refusing `.git` internals and parents that are symlinks leading outside.
pub fn worktree_path(workdir: &Path, path: &BStr) -> anyhow::Result<PathBuf> {
    if path.split(|b| *b == b'/').next() == Some(b".git".as_slice()) {
        bail!("`{path}` is inside `.git`; read repository data through the other tools instead");
    }
    let full = workdir.join(gix::path::from_bstr(path)?);
    let root = workdir.canonicalize()?;
    let parent = full.parent().unwrap_or(&root);
    let real_parent = parent
        .canonicalize()
        .with_context(|| format!("`{path}` does not exist in the worktree"))?;
    if !real_parent.starts_with(&root) {
        bail!(
            "`{path}` resolves outside the repository through a symlinked directory; refusing to read it"
        );
    }
    Ok(full)
}

fn octal_mode(kind: EntryKind) -> String {
    format!("{:06o}", kind as u16)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTreeArgs {
    /// Revision whose tree to list (default `HEAD`): any commit-ish, or a tree SHA.
    pub rev: Option<String>,
    /// Directory to list, relative to the repository root (default: the root). A file path lists just that entry.
    pub path: Option<String>,
    /// List everything below `path` instead of only its direct children (default false). Like
    /// `git ls-tree -r`, recursive listings contain files, symlinks and submodules but not directories.
    #[serde(default)]
    pub recursive: bool,
    /// Maximum entries to return (default 500, max 5000).
    pub limit: Option<usize>,
    /// Number of entries to skip first, for pagination.
    pub skip: Option<usize>,
}

/// What a tree entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TreeEntryKind {
    /// A regular or executable file.
    Blob,
    /// A directory.
    Tree,
    /// A submodule; `id` is the commit it records.
    Commit,
    Symlink,
}

impl From<EntryKind> for TreeEntryKind {
    fn from(kind: EntryKind) -> Self {
        match kind {
            EntryKind::Tree => Self::Tree,
            EntryKind::Blob | EntryKind::BlobExecutable => Self::Blob,
            EntryKind::Link => Self::Symlink,
            EntryKind::Commit => Self::Commit,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TreeEntry {
    /// Path relative to the repository root.
    pub path: String,
    /// What the entry is: file, directory, symlink or submodule.
    pub kind: TreeEntryKind,
    /// Git file mode in octal: `100644` file, `100755` executable, `120000` symlink, `040000` directory, `160000` submodule.
    pub mode: String,
    /// Full SHA of the object (for a submodule, the recorded commit).
    pub id: String,
    /// Size in bytes for files and symlinks; `null` for directories and submodules.
    pub size: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListTreeResult {
    /// Entries in git's tree order (the same order as `git ls-tree`).
    pub entries: Vec<TreeEntry>,
    /// Number of entries in the full listing, before pagination.
    pub total: usize,
    /// True if more entries exist past this page.
    pub truncated: bool,
}

/// An entry found while listing, before its size is looked up.
pub struct Listed {
    pub path: gix::bstr::BString,
    pub kind: EntryKind,
    pub id: gix::ObjectId,
}

pub fn list_tree(repo: &gix::Repository, args: ListTreeArgs) -> anyhow::Result<ListTreeResult> {
    let rev = args.rev.as_deref().unwrap_or("HEAD");
    let path = repo_path(args.path.as_deref().unwrap_or_default())?;
    let root = resolve_tree(repo, args.rev.as_deref())?;
    let listed = if path.is_empty() {
        list_children(&root, "".into(), args.recursive)?
    } else {
        let entry = root
            .lookup_entry(path.split(|b| *b == b'/'))?
            .with_context(|| format!("`{path}` does not exist at `{rev}`; call list_tree without `path` to see the root"))?;
        if entry.mode().is_tree() {
            list_children(&entry.object()?.into_tree(), path.as_ref(), args.recursive)?
        } else {
            vec![Listed {
                kind: entry.mode().kind(),
                id: entry.object_id(),
                path,
            }]
        }
    };

    let total = listed.len();
    let skip = args.skip.unwrap_or(0).min(total);
    let limit = page_limit(args.limit, DEFAULT_TREE_LIMIT, MAX_TREE_LIMIT);
    let entries = listed[skip..]
        .iter()
        .take(limit)
        .map(|item| to_tree_entry(repo, item))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(ListTreeResult {
        truncated: skip + entries.len() < total,
        entries,
        total,
    })
}

/// Children of `tree` with paths prefixed by `prefix`; with `recursive`, every non-tree entry below it.
pub fn list_children(
    tree: &gix::Tree<'_>,
    prefix: &BStr,
    recursive: bool,
) -> anyhow::Result<Vec<Listed>> {
    let join = |name: &BStr| -> gix::bstr::BString {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            let mut path = prefix.to_owned();
            path.push(b'/');
            path.extend_from_slice(name);
            path
        }
    };
    if !recursive {
        return tree
            .iter()
            .map(|entry| {
                let entry = entry?;
                Ok(Listed {
                    path: join(entry.filename()),
                    kind: entry.mode().kind(),
                    id: entry.object_id(),
                })
            })
            .collect();
    }
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse().depthfirst(&mut recorder)?;
    Ok(recorder
        .records
        .into_iter()
        .filter(|r| !r.mode.is_tree())
        .map(|r| Listed {
            path: join(r.filepath.as_ref()),
            kind: r.mode.kind(),
            id: r.oid,
        })
        .collect())
}

fn to_tree_entry(repo: &gix::Repository, item: &Listed) -> anyhow::Result<TreeEntry> {
    let size = match item.kind {
        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
            Some(repo.find_header(item.id)?.size())
        }
        EntryKind::Tree | EntryKind::Commit => None,
    };
    Ok(TreeEntry {
        path: item.path.to_string(),
        kind: item.kind.into(),
        mode: octal_mode(item.kind),
        id: item.id.to_string(),
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git, write,
    };

    fn read(
        repo: &gix::Repository,
        path: &str,
        rev: Option<&str>,
    ) -> anyhow::Result<ReadFileResult> {
        read_file(
            repo,
            ReadFileArgs {
                path: path.into(),
                rev: rev.map(Into::into),
                offset_bytes: None,
                max_bytes: None,
            },
        )
    }

    fn tree_args(path: Option<&str>, recursive: bool) -> ListTreeArgs {
        ListTreeArgs {
            rev: None,
            path: path.map(Into::into),
            recursive,
            limit: None,
            skip: None,
        }
    }

    /// Nested directories, an executable, a symlink, a binary file and a unicode name.
    fn tree_fixture() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "src/main.rs", "fn main() {}\n");
        write(root, "src/util/mod.rs", "pub fn x() {}\n");
        write(
            root,
            "src-extra.txt",
            "sorts between src and src/ in git order\n",
        );
        write(root, "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(
            root.join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        write(root, "img.bin", [0u8, 159, 146, 150, 0, 1]);
        write(root, "dïr/ünï.txt", "unicode\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "tree fixture"]);
        let repo = open(root);
        (dir, repo)
    }

    #[test]
    fn reads_text_at_head_and_older_revisions() {
        let (_dir, repo) = fixture_repo();
        let res = read(&repo, "a.txt", None).unwrap();
        assert_eq!(res.content.as_deref(), Some("one\ntwo\n"));
        assert!(!res.binary && !res.truncated);
        assert_eq!(res.size, 8);
        assert_eq!(res.mode, "100644");
        let old = read(&repo, "./a.txt", Some("HEAD~1")).unwrap();
        assert_eq!(old.content.as_deref(), Some("one\n"));
        assert_ne!(old.blob_id, res.blob_id);
    }

    #[test]
    fn blob_id_matches_git() {
        let (dir, repo) = fixture_repo();
        let res = read(&repo, "a.txt", Some("v1")).unwrap();
        assert_eq!(
            res.blob_id.unwrap(),
            git_stdout(dir.path(), &["rev-parse", "v1:a.txt"]).trim()
        );
    }

    #[test]
    fn reads_index_and_worktree() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        write(root, "a.txt", "staged\n");
        run_git(root, &["add", "a.txt"]);
        write(root, "a.txt", "on disk\n");
        write(root, "untracked.txt", "new\n");
        assert_eq!(
            read(&repo, "a.txt", Some("index"))
                .unwrap()
                .content
                .as_deref(),
            Some("staged\n")
        );
        let wt = read(&repo, "a.txt", Some("worktree")).unwrap();
        assert_eq!(wt.content.as_deref(), Some("on disk\n"));
        assert!(wt.blob_id.is_none());
        assert_eq!(
            read(&repo, "untracked.txt", Some("worktree")).unwrap().size,
            4
        );
        let err = read(&repo, "untracked.txt", Some("index"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in the index"), "{err}");
    }

    #[test]
    fn binary_content_is_base64() {
        let (_dir, repo) = tree_fixture();
        let res = read(&repo, "img.bin", None).unwrap();
        assert!(res.binary);
        assert!(res.content.is_none());
        assert_eq!(res.base64.as_deref(), Some("AJ+SlgAB"));
    }

    #[test]
    fn symlinks_unicode_and_executables() {
        let (_dir, repo) = tree_fixture();
        let link = read(&repo, "link", None).unwrap();
        assert_eq!(link.content.as_deref(), Some("a.txt"));
        assert_eq!(link.mode, "120000");
        let wt_link = read(&repo, "link", Some("worktree")).unwrap();
        assert_eq!(wt_link.content.as_deref(), Some("a.txt"));
        assert_eq!(wt_link.mode, "120000");
        assert_eq!(
            read(&repo, "dïr/ünï.txt", None).unwrap().content.as_deref(),
            Some("unicode\n")
        );
        assert_eq!(read(&repo, "run.sh", None).unwrap().mode, "100755");
        assert_eq!(
            read(&repo, "run.sh", Some("worktree")).unwrap().mode,
            "100755"
        );
    }

    #[test]
    fn chunked_reads_respect_char_boundaries() {
        let (dir, _) = fixture_repo();
        write(dir.path(), "utf8.txt", "aé€😀z");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "utf8"]);
        let repo = open(dir.path());
        let mut text = String::new();
        let mut offset = Some(0);
        while let Some(at) = offset {
            let res = read_file(
                &repo,
                ReadFileArgs {
                    path: "utf8.txt".into(),
                    rev: None,
                    offset_bytes: Some(at),
                    max_bytes: Some(4),
                },
            )
            .unwrap();
            assert!(!res.binary, "chunk at {at} must stay text");
            text.push_str(res.content.as_deref().unwrap());
            assert_eq!(res.truncated, res.next_offset.is_some());
            offset = res.next_offset;
        }
        assert_eq!(text, "aé€😀z");
    }

    #[test]
    fn read_errors_are_actionable() {
        let (dir, repo) = tree_fixture();
        let msg = |r: anyhow::Result<ReadFileResult>| format!("{:#}", r.unwrap_err());
        assert!(msg(read(&repo, "missing.txt", None)).contains("does not exist"));
        assert!(msg(read(&repo, "src", None)).contains("is a directory"));
        assert!(msg(read(&repo, "src", Some("worktree"))).contains("is a directory"));
        assert!(msg(read(&repo, "a.txt", Some("nope"))).contains("cannot resolve"));
        assert!(msg(read(&repo, ".git/config", Some("worktree"))).contains("inside `.git`"));
        assert!(msg(read(&repo, "../etc/passwd", Some("worktree"))).contains(".."));
        let past_end = read_file(
            &repo,
            ReadFileArgs {
                path: "a.txt".into(),
                rev: None,
                offset_bytes: Some(1000),
                max_bytes: None,
            },
        );
        assert!(msg(past_end).contains("past the end"));

        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.txt", "s");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        assert!(
            msg(read(&repo, "escape/secret.txt", Some("worktree")))
                .contains("outside the repository")
        );
    }

    #[test]
    fn conflicted_index_entry_points_to_worktree() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "ours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!crate::tools::testutil::try_git(
            root,
            &["merge", "-q", "feature"]
        ));
        let err = read(&open(root), "a.txt", Some("index"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("merge conflicts"), "{err}");
    }

    #[test]
    fn bare_and_empty_repos() {
        let (_dir, bare) = bare_repo();
        assert_eq!(
            read(&bare, "a.txt", None).unwrap().content.as_deref(),
            Some("one\ntwo\n")
        );
        assert!(
            read(&bare, "a.txt", Some("worktree"))
                .unwrap_err()
                .to_string()
                .contains("bare")
        );
        let (_dir, empty) = empty_repo();
        assert!(
            format!("{:#}", read(&empty, "a.txt", None).unwrap_err()).contains("no commits yet")
        );
    }

    /// `(mode, type, id, path)` rows from `git ls-tree`, the oracle for list_tree.
    fn git_ls_tree(dir: &std::path::Path, args: &[&str]) -> Vec<(String, String, String, String)> {
        let mut cmd = vec!["-c", "core.quotePath=false", "ls-tree"];
        cmd.extend_from_slice(args);
        git_stdout(dir, &cmd)
            .lines()
            .map(|line| {
                let (meta, path) = line.split_once('\t').unwrap();
                let f: Vec<_> = meta.split(' ').collect();
                (
                    f[0].to_string(),
                    f[1].to_string(),
                    f[2].to_string(),
                    path.to_string(),
                )
            })
            .collect()
    }

    fn rows(res: &ListTreeResult) -> Vec<(String, String, String, String)> {
        res.entries
            .iter()
            .map(|e| {
                let kind = match e.kind {
                    TreeEntryKind::Tree => "tree",
                    TreeEntryKind::Commit => "commit",
                    TreeEntryKind::Blob | TreeEntryKind::Symlink => "blob",
                };
                (
                    e.mode.clone(),
                    kind.to_string(),
                    e.id.clone(),
                    e.path.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn list_tree_matches_ls_tree() {
        let (dir, repo) = tree_fixture();
        let root = dir.path();
        let cases: [(Option<&str>, bool, &[&str]); 5] = [
            (None, false, &["HEAD"]),
            (None, true, &["-r", "HEAD"]),
            (Some("src"), false, &["HEAD", "src/"]),
            (Some("src/"), true, &["-r", "HEAD", "src/"]),
            (Some("src/main.rs"), false, &["HEAD", "src/main.rs"]),
        ];
        for (path, recursive, git_args) in cases {
            let res = list_tree(&repo, tree_args(path, recursive)).unwrap();
            assert_eq!(
                rows(&res),
                git_ls_tree(root, git_args),
                "{path:?} recursive={recursive}"
            );
            assert_eq!(res.total, res.entries.len());
        }
    }

    #[test]
    fn list_tree_kinds_and_sizes() {
        let (_dir, repo) = tree_fixture();
        let res = list_tree(&repo, tree_args(None, false)).unwrap();
        let find = |p: &str| res.entries.iter().find(|e| e.path == p).unwrap();
        assert_eq!(find("src").kind, TreeEntryKind::Tree);
        assert_eq!(find("src").size, None);
        assert_eq!(find("link").kind, TreeEntryKind::Symlink);
        assert_eq!(find("run.sh").mode, "100755");
        assert_eq!(find("a.txt").size, Some(8));
        let nested = list_tree(&repo, tree_args(Some("dïr"), false)).unwrap();
        assert_eq!(nested.entries[0].path, "dïr/ünï.txt");
    }

    #[test]
    fn list_tree_paginates() {
        let (_dir, repo) = tree_fixture();
        let all = list_tree(&repo, tree_args(None, true)).unwrap();
        let mut args = tree_args(None, true);
        args.limit = Some(3);
        args.skip = Some(2);
        let page = list_tree(&repo, args).unwrap();
        assert_eq!(page.entries.len(), 3);
        assert_eq!(page.entries[0].path, all.entries[2].path);
        assert!(page.truncated);
        let mut args = tree_args(None, true);
        args.skip = Some(all.total - 1);
        let last = list_tree(&repo, args).unwrap();
        assert_eq!(last.entries.len(), 1);
        assert!(!last.truncated);
    }

    #[test]
    fn list_tree_submodule_and_errors() {
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
        let repo = open(root);
        let res = list_tree(&repo, tree_args(None, false)).unwrap();
        let sub_entry = res.entries.iter().find(|e| e.path == "sub").unwrap();
        assert_eq!(sub_entry.kind, TreeEntryKind::Commit);
        assert_eq!(sub_entry.mode, "160000");
        assert!(
            read(&repo, "sub", None)
                .unwrap_err()
                .to_string()
                .contains("submodule")
        );
        let err = format!(
            "{:#}",
            list_tree(&repo, tree_args(Some("nope"), false)).unwrap_err()
        );
        assert!(err.contains("does not exist"), "{err}");
    }
}
