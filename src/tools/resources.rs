//! Repository content as MCP resources: commits, files and diffs addressed by `gitrs://` URIs, so
//! a client can attach them as context, plus completion of revisions and paths for their arguments.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
};

use anyhow::{Context, bail};
use gix::{
    bstr::{BStr, ByteSlice},
    prelude::ObjectIdExt,
};
use percent_encoding::percent_decode_str;

use super::{
    MAX_MAX_BYTES,
    diff::{DiffArgs, DiffFormat, DiffResult, diff},
    files::{ReadFileArgs, list_children, read_file},
    history::{ShowCommitArgs, show_commit},
    repo_path, resolve_tree,
};

pub const COMMIT_TEMPLATE: &str = "gitrs://commit/{rev}";
pub const FILE_TEMPLATE: &str = "gitrs://file/{rev}/{+path}";
pub const DIFF_TEMPLATE: &str = "gitrs://diff/{from}..{to}";

const SCHEME: &str = "gitrs://";
/// Files listed in a diff or commit resource; past this, a note says how many were left out.
const MAX_FILES: usize = 1000;
/// Most completion values one response may carry, per the MCP specification.
const MAX_COMPLETIONS: usize = 100;

/// A resource the server lists up front, as well as the templates.
pub struct FixedResource {
    pub uri: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
}

/// A resource template with what its arguments mean, for `resources/templates/list`.
pub struct Template {
    pub uri_template: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub mime_type: Option<&'static str>,
}

pub const TEMPLATES: [Template; 3] = [
    Template {
        uri_template: COMMIT_TEMPLATE,
        name: "commit",
        title: "Commit",
        description: "A commit as `git show` prints it: header, message and the patch against its first \
                      parent. `rev` is any revision; percent-encode `/` in it as `%2F`.",
        mime_type: Some("text/x-diff"),
    },
    Template {
        uri_template: FILE_TEMPLATE,
        name: "file",
        title: "File",
        description: "A file's content at a revision, or `index` (staged) or `worktree` (on disk). \
                      Text comes back as text, anything else as a base64 blob; files over 1 MiB are \
                      refused (use the read_file tool to read them in chunks). Percent-encode `/` in `rev`.",
        mime_type: None,
    },
    Template {
        uri_template: DIFF_TEMPLATE,
        name: "diff",
        title: "Diff",
        description: "A unified diff, like `git diff`: `<rev>..<rev>` between two revisions, \
                      `<rev>..index` for changes staged on top of a revision, or `index..worktree` for \
                      unstaged changes. Percent-encode `/` in revisions.",
        mime_type: Some("text/x-diff"),
    },
];

/// Resources worth listing up front. Staged and unstaged changes need a working tree.
pub fn fixed(repo: &gix::Repository) -> Vec<FixedResource> {
    let mut list = vec![FixedResource {
        uri: "gitrs://commit/HEAD",
        name: "head",
        title: "HEAD commit",
        description: "The commit HEAD points to, with its patch.",
        mime_type: "text/x-diff",
    }];
    if repo.workdir().is_some() {
        list.push(FixedResource {
            uri: "gitrs://diff/HEAD..index",
            name: "staged",
            title: "Staged changes",
            description: "What the next commit would contain, like `git diff --cached`.",
            mime_type: "text/x-diff",
        });
        list.push(FixedResource {
            uri: "gitrs://diff/index..worktree",
            name: "unstaged",
            title: "Unstaged changes",
            description: "Changes in the working tree that aren't staged, like `git diff`.",
            mime_type: "text/x-diff",
        });
    }
    list
}

/// What a `gitrs://` URI addresses.
#[derive(Debug, PartialEq, Eq)]
pub enum Address {
    Commit { rev: String },
    File { rev: String, path: String },
    Diff { from: String, to: String },
}

impl Address {
    pub fn parse(uri: &str) -> anyhow::Result<Self> {
        let rest = uri.strip_prefix(SCHEME).with_context(|| {
            format!("`{uri}` isn't a gitrs resource; URIs start with `{SCHEME}`")
        })?;
        let (kind, rest) = rest.split_once('/').unwrap_or((rest, ""));
        match kind {
            "commit" => Ok(Self::Commit {
                rev: decode_rev(rest)?,
            }),
            "file" => {
                let (rev, path) = rest
                    .split_once('/')
                    .filter(|(rev, path)| !rev.is_empty() && !path.is_empty())
                    .with_context(|| {
                        format!("`{uri}` needs a revision and a path: {FILE_TEMPLATE}")
                    })?;
                Ok(Self::File {
                    rev: decode_rev(rev)?,
                    path: decode(path)?,
                })
            }
            "diff" => {
                let (from, to) = rest
                    .split_once("..")
                    .with_context(|| format!("`{uri}` needs two sides: {DIFF_TEMPLATE}"))?;
                Ok(Self::Diff {
                    from: decode_rev(from)?,
                    to: decode_rev(to)?,
                })
            }
            _ => bail!(
                "unknown gitrs resource `{uri}`; the templates are {COMMIT_TEMPLATE}, {FILE_TEMPLATE} and \
                 {DIFF_TEMPLATE}"
            ),
        }
    }
}

fn decode(text: &str) -> anyhow::Result<String> {
    Ok(percent_decode_str(text)
        .decode_utf8()
        .with_context(|| format!("`{text}` isn't valid percent-encoded UTF-8"))?
        .into_owned())
}

fn decode_rev(text: &str) -> anyhow::Result<String> {
    let rev = decode(text)?;
    if rev.is_empty() {
        bail!("the revision is missing; pass one such as `HEAD` or a branch name");
    }
    Ok(rev)
}

/// A resource's content: text, or binary data already base64-encoded.
#[derive(Debug)]
pub enum Body {
    Text(String),
    Base64(String),
}

#[derive(Debug)]
pub struct Contents {
    pub mime_type: &'static str,
    pub body: Body,
}

pub fn read(repo: &gix::Repository, uri: &str) -> anyhow::Result<Contents> {
    match Address::parse(uri)? {
        Address::Commit { rev } => Ok(Contents {
            mime_type: "text/x-diff",
            body: Body::Text(commit_text(repo, &rev)?),
        }),
        Address::File { rev, path } => file(repo, rev, path),
        Address::Diff { from, to } => Ok(Contents {
            mime_type: "text/x-diff",
            body: Body::Text(render(&diff(repo, diff_args(&from, &to, MAX_MAX_BYTES)?)?)),
        }),
    }
}

/// The commit as `git show` prints it by default, with paths unquoted as with `core.quotePath=false`.
/// A merge's patch is against its first parent: git shows a combined diff, which can't be applied.
fn commit_text(repo: &gix::Repository, rev: &str) -> anyhow::Result<String> {
    let detail = show_commit(
        repo,
        ShowCommitArgs {
            rev: Some(rev.to_owned()),
        },
    )?;
    let mut text = format!("commit {}\n", detail.id);
    if detail.parents.len() > 1 {
        let short = detail
            .parents
            .iter()
            .map(|p| {
                Ok(gix::ObjectId::from_hex(p.as_bytes())?
                    .attach(repo)
                    .shorten_or_id()
                    .to_string())
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        text.push_str(&format!("Merge: {}\n", short.join(" ")));
    }
    let author = &detail.author;
    let date = gix::date::Time::new(author.time, author.offset)
        .format_or_unix(gix::date::time::format::DEFAULT);
    text.push_str(&format!(
        "Author: {} <{}>\nDate:   {date}\n\n",
        author.name, author.email
    ));
    for line in detail.message.trim_end().lines() {
        text.push_str(&format!("    {line}\n"));
    }
    let from = match detail.parents.first() {
        Some(parent) => parent.clone(),
        None => repo.empty_tree().id.to_string(),
    };
    let args = patch_args(Some(from), Some(detail.id), false, MAX_MAX_BYTES);
    let patch = render(&diff(repo, args)?);
    if !patch.is_empty() {
        text.push('\n');
        text.push_str(&patch);
    }
    Ok(text)
}

/// Patch-format arguments for the sides `from..to`: two revisions, a revision and `index`, or
/// `index..worktree`. Patch text stops at `max_bytes`.
pub fn diff_args(from: &str, to: &str, max_bytes: usize) -> anyhow::Result<DiffArgs> {
    Ok(match (from, to) {
        ("index", "worktree") => patch_args(None, None, false, max_bytes),
        ("index" | "worktree", _) | (_, "worktree") => bail!(
            "unsupported diff `{from}..{to}`; use `<rev>..<rev>`, `<rev>..index` or `index..worktree`"
        ),
        // Leaving HEAD to the diff tool's default lets it stand for the empty tree before the first
        // commit, so the staged changes of a new repository still read.
        ("HEAD", "index") => patch_args(None, None, true, max_bytes),
        (_, "index") => patch_args(Some(from.to_owned()), None, true, max_bytes),
        _ => patch_args(Some(from.to_owned()), Some(to.to_owned()), false, max_bytes),
    })
}

pub fn patch_args(
    from: Option<String>,
    to: Option<String>,
    staged: bool,
    max_bytes: usize,
) -> DiffArgs {
    DiffArgs {
        from,
        to,
        staged,
        pathspec: Vec::new(),
        format: DiffFormat::Patch,
        context_lines: None,
        max_bytes: Some(max_bytes),
        limit: Some(MAX_FILES),
        skip: None,
    }
}

/// Every file's patch in order, then a note on anything the byte or file budget left out. git apply
/// ignores text after the last hunk, so the note doesn't break the patch.
pub fn render(result: &DiffResult) -> String {
    let mut text: String = result
        .files
        .iter()
        .filter_map(|f| f.patch.as_deref())
        .collect();
    let cut = result
        .files
        .iter()
        .filter(|f| f.patch.is_none() || f.patch_truncated)
        .count();
    let left_out = result.totals.files - result.files.len() + cut;
    if result.truncated {
        text.push_str(&format!(
            "\n# {left_out} of {} files are missing or cut short here; use the diff tool, which pages \
             through them\n",
            result.totals.files
        ));
    }
    if !result.unmerged.is_empty() {
        text.push_str(&format!(
            "\n# Unmerged, not diffed: {}\n",
            result.unmerged.join(", ")
        ));
    }
    text
}

fn file(repo: &gix::Repository, rev: String, path: String) -> anyhow::Result<Contents> {
    let read = read_file(
        repo,
        ReadFileArgs {
            path,
            rev: Some(rev),
            offset_bytes: None,
            max_bytes: Some(MAX_MAX_BYTES),
        },
    )?;
    if read.truncated {
        bail!(
            "`{}` is {} bytes, over the 1 MiB resource limit; read it in chunks with the read_file tool",
            read.path,
            read.size
        );
    }
    let contents = match (read.content, read.base64) {
        (Some(text), _) => Contents {
            mime_type: "text/plain",
            body: Body::Text(text),
        },
        (None, Some(data)) => Contents {
            mime_type: "application/octet-stream",
            body: Body::Base64(data),
        },
        (None, None) => bail!("`{}` came back without content", read.path),
    };
    Ok(contents)
}

/// A value that changes whenever the content of `uri` may have changed, cheap enough to poll: the
/// object a revision side resolves to and the index file's size and modification time. A working
/// tree side has no such summary, so its content is hashed. A URI that can't be read hashes its
/// error, so becoming readable (or unreadable) counts as a change too.
pub fn fingerprint(repo: &gix::Repository, uri: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    match state(repo, uri) {
        Ok(parts) => parts.hash(&mut hasher),
        Err(err) => format!("{err:#}").hash(&mut hasher),
    }
    hasher.finish()
}

fn state(repo: &gix::Repository, uri: &str) -> anyhow::Result<Vec<String>> {
    Ok(match Address::parse(uri)? {
        Address::Commit { rev } => vec![side_state(repo, &rev)?],
        Address::File { rev, path } if rev == "worktree" => match read(repo, uri)?.body {
            Body::Text(text) | Body::Base64(text) => vec![path, text],
        },
        Address::File { rev, .. } => vec![side_state(repo, &rev)?],
        Address::Diff { from, to } if to == "worktree" => {
            vec![render(&diff(repo, diff_args(&from, &to, MAX_MAX_BYTES)?)?)]
        }
        Address::Diff { from, to } => vec![side_state(repo, &from)?, side_state(repo, &to)?],
    })
}

/// What one side of a resource currently is: the index file's stamp, or the object a revision names.
fn side_state(repo: &gix::Repository, side: &str) -> anyhow::Result<String> {
    if side == "index" {
        return Ok(match std::fs::metadata(repo.index_path()) {
            Ok(meta) => format!("index {} {:?}", meta.len(), meta.modified().ok()),
            Err(_) => "no index".to_owned(),
        });
    }
    Ok(repo.rev_parse_single(side)?.to_string())
}

/// Completion values for one argument, at most 100, ordered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Completion {
    pub values: Vec<String>,
    /// Number of matches, including any beyond the values returned.
    pub total: usize,
}

/// Complete the argument `name` from what's been typed (`value`). `context` holds arguments already
/// filled in: a path completes against `rev` (or `from`), default `HEAD`. Unknown arguments get no
/// values.
pub fn complete(
    repo: &gix::Repository,
    name: &str,
    value: &str,
    context: &HashMap<String, String>,
) -> anyhow::Result<Completion> {
    let candidates = match name {
        "rev" => revisions(repo, &["index", "worktree"])?,
        "from" => revisions(repo, &["index"])?,
        "to" => revisions(repo, &["index", "worktree"])?,
        "path" => {
            let rev = context.get("rev").or_else(|| context.get("from"));
            paths(repo, rev.map(String::as_str), value)?
        }
        _ => Vec::new(),
    };
    Ok(matching(candidates, value))
}

/// The `candidates` starting with `typed`, sorted, without repeats, and at most 100.
pub fn matching(candidates: Vec<String>, typed: &str) -> Completion {
    let mut values: Vec<String> = candidates
        .into_iter()
        .filter(|c| c.starts_with(typed))
        .collect();
    values.sort();
    values.dedup();
    let total = values.len();
    values.truncate(MAX_COMPLETIONS);
    Completion { values, total }
}

/// `HEAD`, the short names of branches, remote branches and tags, and `special` words.
fn revisions(repo: &gix::Repository, special: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = special.iter().map(|s| (*s).to_owned()).collect();
    names.push("HEAD".to_owned());
    let refs = repo.references()?;
    let kinds = [
        refs.local_branches()?,
        refs.remote_branches()?,
        refs.tags()?,
    ];
    for reference in kinds.into_iter().flatten() {
        let reference = reference.map_err(|e| anyhow::anyhow!(e))?;
        names.push(reference.name().shorten().to_string());
    }
    Ok(names)
}

/// Entries of the directory `typed` is in, at `rev` (default `HEAD`), with `/` after directories.
/// For `index` and `worktree`, paths come from the index.
fn paths(repo: &gix::Repository, rev: Option<&str>, typed: &str) -> anyhow::Result<Vec<String>> {
    let dir = typed.rfind('/').map_or("", |at| &typed[..at]);
    match rev {
        Some("index" | "worktree") => index_paths(repo, dir),
        rev => tree_paths(repo, rev, dir),
    }
}

fn tree_paths(repo: &gix::Repository, rev: Option<&str>, dir: &str) -> anyhow::Result<Vec<String>> {
    let root = resolve_tree(repo, rev)?;
    let dir = repo_path(dir)?;
    let tree = if dir.is_empty() {
        root
    } else {
        match root.lookup_entry(dir.split(|b| *b == b'/'))? {
            Some(entry) if entry.mode().is_tree() => entry.object()?.into_tree(),
            _ => return Ok(Vec::new()),
        }
    };
    Ok(list_children(&tree, dir.as_ref(), false)?
        .into_iter()
        .map(|item| {
            with_dir_slash(
                item.path.as_ref(),
                item.kind == gix::object::tree::EntryKind::Tree,
            )
        })
        .collect())
}

fn index_paths(repo: &gix::Repository, dir: &str) -> anyhow::Result<Vec<String>> {
    let index = repo.index_or_empty()?;
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut found = Vec::new();
    for entry in index.entries() {
        let path = entry.path(&index);
        let Some(rest) = path.strip_prefix(prefix.as_bytes()) else {
            continue;
        };
        // Only the next level down: `src/main.rs` lists as `src/` from the root.
        let name = match rest.find_byte(b'/') {
            Some(at) => with_dir_slash(path[..prefix.len() + at].as_bstr(), true),
            None => path.to_string(),
        };
        found.push(name);
    }
    Ok(found)
}

fn with_dir_slash(path: &BStr, directory: bool) -> String {
    if directory {
        format!("{path}/")
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{
        bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git, shallow_clone, try_git,
        write,
    };

    fn text(contents: Contents) -> String {
        match contents.body {
            Body::Text(text) => text,
            Body::Base64(_) => panic!("expected text"),
        }
    }

    fn read_text(repo: &gix::Repository, uri: &str) -> String {
        text(read(repo, uri).unwrap())
    }

    fn error(repo: &gix::Repository, uri: &str) -> String {
        read(repo, uri).unwrap_err().to_string()
    }

    /// The fixture plus a root commit to compare against, a new file, a blank line in a message, a
    /// branch with a `/` and a unicode file name.
    fn history(root: &Path) {
        write(root, "src/lib.rs", "pub fn f() {}\n");
        write(root, "docs/ünï.md", "# hi\n");
        run_git(root, &["add", "."]);
        run_git(
            root,
            &["commit", "-q", "-m", "add lib\n\nfirst paragraph\n\nsecond"],
        );
        run_git(root, &["branch", "topic/x"]);
    }

    #[test]
    fn commits_read_like_git_show() {
        let (dir, repo) = fixture_repo();
        history(dir.path());
        for rev in ["HEAD", "HEAD~1", "HEAD~2", "v1", "topic%2Fx"] {
            let git_rev = rev.replace("%2F", "/");
            assert_eq!(
                read_text(&repo, &format!("gitrs://commit/{rev}")),
                git_stdout(
                    dir.path(),
                    &["-c", "core.quotePath=false", "show", "--no-color", &git_rev]
                ),
                "{rev}"
            );
        }
        assert_eq!(
            read(&repo, "gitrs://commit/HEAD").unwrap().mime_type,
            "text/x-diff"
        );
    }

    #[test]
    fn shallow_boundary_commits_show_like_root_commits() {
        let (source, _) = fixture_repo();
        let (dir, repo) = shallow_clone(source.path());
        assert_eq!(
            read_text(&repo, "gitrs://commit/HEAD"),
            git_stdout(dir.path(), &["show", "--no-color", "HEAD"])
        );
    }

    #[test]
    fn merge_commits_show_the_first_parent_patch() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "b.txt", "bee\n");
        run_git(root, &["add", "b.txt"]);
        run_git(root, &["commit", "-q", "-m", "add b"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "c.txt", "sea\n");
        run_git(root, &["add", "c.txt"]);
        run_git(root, &["commit", "-q", "-m", "add c"]);
        run_git(root, &["merge", "-q", "--no-edit", "feature"]);
        let ours = read_text(&repo, "gitrs://commit/HEAD");
        let git = git_stdout(root, &["show", "--no-color", "--no-patch", "HEAD"]);
        let first_parent = git_stdout(root, &["diff", "HEAD^1", "HEAD"]);
        assert_eq!(ours, format!("{git}\n{first_parent}"));
        assert!(ours.contains("\nMerge: "));
    }

    #[test]
    fn diffs_read_like_git_diff() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        history(root);
        write(root, "a.txt", "staged\n");
        run_git(root, &["add", "a.txt"]);
        write(root, "a.txt", "staged\nand not\n");
        let cases: [(&str, &[&str]); 4] = [
            ("HEAD~2..HEAD", &["diff", "HEAD~2", "HEAD"]),
            ("v1..topic%2Fx", &["diff", "v1", "topic/x"]),
            ("HEAD..index", &["diff", "--cached"]),
            ("index..worktree", &["diff"]),
        ];
        for (sides, git) in cases {
            assert_eq!(
                read_text(&repo, &format!("gitrs://diff/{sides}")),
                git_stdout(root, &[&["-c", "core.quotePath=false"], git].concat()),
                "{sides}"
            );
        }
        assert_eq!(read_text(&repo, "gitrs://diff/HEAD..HEAD"), "");
        for bad in ["worktree..index", "index..HEAD", "HEAD..worktree"] {
            assert!(
                error(&repo, &format!("gitrs://diff/{bad}")).contains("index..worktree"),
                "{bad}"
            );
        }
    }

    #[test]
    fn staged_changes_read_before_the_first_commit() {
        let (dir, repo) = empty_repo();
        write(dir.path(), "first.txt", "hello\n");
        run_git(dir.path(), &["add", "first.txt"]);
        assert_eq!(
            read_text(&repo, "gitrs://diff/HEAD..index"),
            git_stdout(dir.path(), &["diff", "--cached"])
        );
    }

    #[test]
    fn unmerged_paths_are_noted() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "ours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        assert!(
            read_text(&repo, "gitrs://diff/index..worktree")
                .ends_with("# Unmerged, not diffed: a.txt\n")
        );
    }

    #[test]
    fn files_at_revisions_index_and_worktree() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        history(root);
        write(root, "a.txt", "staged\n");
        run_git(root, &["add", "a.txt"]);
        write(root, "a.txt", "on disk\n");
        let cases = [
            ("HEAD/a.txt", "one\ntwo\n"),
            ("HEAD~2/a.txt", "one\n"),
            ("index/a.txt", "staged\n"),
            ("worktree/a.txt", "on disk\n"),
            ("topic%2Fx/src/lib.rs", "pub fn f() {}\n"),
            ("HEAD/docs/%C3%BCn%C3%AF.md", "# hi\n"),
            ("HEAD/docs/ünï.md", "# hi\n"),
        ];
        for (rest, expected) in cases {
            let contents = read(&repo, &format!("gitrs://file/{rest}")).unwrap();
            assert_eq!(contents.mime_type, "text/plain");
            assert_eq!(text(contents), expected, "{rest}");
        }

        write(root, "bin.dat", b"\0\x01\x02");
        let blob = read(&repo, "gitrs://file/worktree/bin.dat").unwrap();
        assert_eq!(blob.mime_type, "application/octet-stream");
        assert!(matches!(blob.body, Body::Base64(ref data) if data == "AAEC"));

        write(root, "big.txt", "x".repeat(MAX_MAX_BYTES + 1));
        assert!(error(&repo, "gitrs://file/worktree/big.txt").contains("read_file"));
        assert!(error(&repo, "gitrs://file/HEAD/missing.txt").contains("missing.txt"));
    }

    #[test]
    fn malformed_uris_say_what_is_expected() {
        let (_dir, repo) = fixture_repo();
        let cases = [
            ("https://example.com/x", "start with `gitrs://`"),
            ("gitrs://blob/HEAD", "unknown gitrs resource"),
            ("gitrs://commit/", "revision is missing"),
            ("gitrs://file/HEAD", "needs a revision and a path"),
            ("gitrs://file//a.txt", "needs a revision and a path"),
            ("gitrs://diff/HEAD", "needs two sides"),
            ("gitrs://diff/..HEAD", "revision is missing"),
            ("gitrs://commit/%FF", "percent-encoded UTF-8"),
            ("gitrs://commit/no-such-rev", "no-such-rev"),
            // Decoding happens before the path checks, so encoded escapes are refused too.
            ("gitrs://file/worktree/%2E%2E/outside", ".."),
            ("gitrs://file/worktree/.GIT/config", "inside `.git`"),
            ("gitrs://file/HEAD/%2Fetc%2Fpasswd", "absolute"),
        ];
        for (uri, message) in cases {
            let err = format!("{:#}", read(&repo, uri).unwrap_err());
            assert!(err.contains(message), "{uri}: {err}");
        }
        assert_eq!(
            Address::parse("gitrs://file/a%2Fb/c%20d/e").unwrap(),
            Address::File {
                rev: "a/b".into(),
                path: "c d/e".into()
            }
        );
    }

    #[test]
    fn fixed_resources_read_and_need_a_worktree_for_changes() {
        let (dir, repo) = fixture_repo();
        let uris: Vec<_> = fixed(&repo).iter().map(|r| r.uri).collect();
        assert_eq!(
            uris,
            [
                "gitrs://commit/HEAD",
                "gitrs://diff/HEAD..index",
                "gitrs://diff/index..worktree"
            ]
        );
        for uri in uris {
            read(&repo, uri).unwrap();
        }
        let (_bare_dir, bare) = bare_repo();
        assert_eq!(fixed(&bare).len(), 1);
        assert!(git_stdout(dir.path(), &["status", "--porcelain"]).is_empty());
        for template in TEMPLATES {
            assert!(template.uri_template.starts_with(SCHEME));
        }
    }

    #[test]
    fn fingerprints_change_with_what_the_resource_shows() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let uris = [
            "gitrs://commit/HEAD",
            "gitrs://commit/v1",
            "gitrs://file/HEAD/a.txt",
            "gitrs://file/index/a.txt",
            "gitrs://file/worktree/a.txt",
            "gitrs://diff/HEAD..index",
            "gitrs://diff/index..worktree",
            "gitrs://commit/no-such-rev",
        ];
        let prints = || -> Vec<u64> { uris.iter().map(|u| fingerprint(&repo, u)).collect() };
        let changed = |before: &[u64], after: &[u64]| -> Vec<&str> {
            uris.iter()
                .zip(before.iter().zip(after))
                .filter(|(_, (a, b))| a != b)
                .map(|(uri, _)| *uri)
                .collect()
        };
        let start = prints();
        assert_eq!(prints(), start, "stable while nothing changes");

        write(root, "a.txt", "edited\n");
        let edited = prints();
        assert_eq!(
            changed(&start, &edited),
            [
                "gitrs://file/worktree/a.txt",
                "gitrs://diff/index..worktree"
            ]
        );

        run_git(root, &["add", "a.txt"]);
        let staged = prints();
        assert_eq!(
            changed(&edited, &staged),
            [
                "gitrs://file/index/a.txt",
                "gitrs://diff/HEAD..index",
                "gitrs://diff/index..worktree"
            ]
        );

        run_git(root, &["commit", "-q", "-m", "third"]);
        let committed = changed(&staged, &prints());
        for uri in [
            "gitrs://commit/HEAD",
            "gitrs://file/HEAD/a.txt",
            "gitrs://diff/HEAD..index",
        ] {
            assert!(committed.contains(&uri), "{uri}: {committed:?}");
        }
        assert!(
            !committed.contains(&"gitrs://commit/v1"),
            "a tag that didn't move"
        );

        let before = prints();
        run_git(root, &["tag", "no-such-rev"]);
        assert_eq!(changed(&before, &prints()), ["gitrs://commit/no-such-rev"]);
    }

    fn values(
        repo: &gix::Repository,
        name: &str,
        value: &str,
        context: &[(&str, &str)],
    ) -> Vec<String> {
        let context = context
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        complete(repo, name, value, &context).unwrap().values
    }

    #[test]
    fn revisions_complete_from_refs() {
        let (dir, repo) = fixture_repo();
        history(dir.path());
        assert_eq!(
            values(&repo, "rev", "", &[]),
            [
                "HEAD", "feature", "index", "main", "topic/x", "v1", "worktree"
            ]
        );
        assert_eq!(
            values(&repo, "from", "", &[]).len(),
            6,
            "no worktree on the old side"
        );
        assert_eq!(values(&repo, "to", "t", &[]), ["topic/x"]);
        assert_eq!(values(&repo, "rev", "ma", &[]), ["main"]);
        assert!(values(&repo, "unknown", "", &[]).is_empty());

        for i in 0..120 {
            run_git(dir.path(), &["tag", &format!("t{i:03}")]);
        }
        let many = complete(&repo, "rev", "t", &HashMap::new()).unwrap();
        assert_eq!((many.values.len(), many.total), (MAX_COMPLETIONS, 121));
    }

    #[test]
    fn paths_complete_one_level_at_a_time() {
        let (dir, repo) = fixture_repo();
        history(dir.path());
        assert_eq!(values(&repo, "path", "", &[]), ["a.txt", "docs/", "src/"]);
        assert_eq!(values(&repo, "path", "s", &[]), ["src/"]);
        assert_eq!(values(&repo, "path", "src/", &[]), ["src/lib.rs"]);
        assert_eq!(values(&repo, "path", "docs/ü", &[]), ["docs/ünï.md"]);
        assert_eq!(values(&repo, "path", "", &[("rev", "HEAD~1")]), ["a.txt"]);
        assert!(values(&repo, "path", "a.txt/", &[]).is_empty());
        assert!(values(&repo, "path", "nope/", &[]).is_empty());

        write(dir.path(), "new/file.rs", "");
        run_git(dir.path(), &["add", "new/file.rs"]);
        assert_eq!(
            values(&repo, "path", "", &[("rev", "index")]),
            ["a.txt", "docs/", "new/", "src/"]
        );
        assert_eq!(
            values(&repo, "path", "new/", &[("from", "worktree")]),
            ["new/file.rs"]
        );
        let reopened = open(dir.path());
        assert!(
            complete(
                &reopened,
                "path",
                "",
                &[("rev".to_owned(), "nope".to_owned())].into()
            )
            .is_err()
        );
    }

    proptest::proptest! {
        /// Parsing never panics, whatever the URI.
        #[test]
        fn any_uri_parses_or_errors(uri in "(gitrs://)?(commit|file|diff|x)?/?[a-z%0-9./?=&]{0,24}") {
            let _ = Address::parse(&uri);
        }

        /// Revisions and paths survive being percent-encoded into a URI and parsed back.
        #[test]
        fn file_uris_round_trip(rev in "[a-z0-9_/~^.-]{1,12}", path in "[a-z0-9_. ü-]{1,8}(/[a-z0-9_.-]{1,8}){0,2}") {
            use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
            let uri = format!("gitrs://file/{}/{}", utf8_percent_encode(&rev, NON_ALPHANUMERIC), path);
            proptest::prop_assert_eq!(Address::parse(&uri).unwrap(), Address::File { rev, path });
        }
    }
}
