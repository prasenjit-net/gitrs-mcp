use anyhow::{Context, bail};
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    index::entry::{Mode, Stage},
    object::tree::EntryKind,
};
use regex::bytes::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    clip_utf8,
    files::{list_children, worktree_path},
    is_binary, page_limit, paginate, pathspec_search,
    progress::Watch,
    resolve_object,
};

const DEFAULT_GREP_LIMIT: usize = 100;
const MAX_GREP_LIMIT: usize = 1000;
/// Matching lines are cut here so one minified line can't fill the response.
const MAX_LINE_BYTES: usize = 512;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Regular expression matched against each line, in Rust `regex` syntax (Perl-like: `\d`, `\b`,
    /// `(?i)`, `a|b`; no backreferences or lookaround). Set `fixed_strings` to search for literal text.
    pub pattern: String,
    /// What to search: `worktree` (default) for tracked files as they are on disk, like `git grep`;
    /// `index` for staged content, like `git grep --cached`; or a revision such as `HEAD`, `main` or
    /// `v1.0`, like `git grep <pattern> <rev>`. A bare repository defaults to `HEAD`.
    pub rev: Option<String>,
    /// Limit the search to these paths or pathspec patterns (e.g. `src/`, `*.rs`,
    /// `:(exclude)tests`), relative to the repository root. Omit or pass `[]` for everything.
    #[serde(default)]
    pub pathspec: Vec<String>,
    /// Match regardless of case, like `git grep -i` (default false).
    #[serde(default)]
    pub ignore_case: bool,
    /// Treat `pattern` as literal text instead of a regex, like `git grep -F` (default false).
    #[serde(default)]
    pub fixed_strings: bool,
    /// Maximum matching lines to return (default 100, max 1000).
    pub limit: Option<usize>,
    /// Number of matching lines to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GrepMatch {
    /// Path relative to the repository root.
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based byte offset in the line where the first match starts, like `git grep --column`.
    pub column: usize,
    /// The matching line without its line terminator, cut at 512 bytes. Invalid UTF-8 is replaced with U+FFFD.
    pub text: String,
    /// True if `text` was cut at 512 bytes.
    pub text_truncated: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GrepResult {
    /// Full SHA of the commit or tree that was searched; `null` when searching the worktree or index.
    pub rev: Option<String>,
    /// Matching lines, ordered by path and then line number.
    pub matches: Vec<GrepMatch>,
    /// True if more matches exist past this page; continue with `skip`.
    pub truncated: bool,
}

/// A file to search, with where its content comes from.
struct Candidate {
    path: BString,
    content: Content,
}

enum Content {
    /// A blob in the object database.
    Blob(ObjectId),
    /// The file on disk, read when it's searched.
    Disk,
}

/// Lines matching a regex in tracked files, like `git grep -n --column -I`.
///
/// Like git, it skips binary files, symlinks, submodules and untracked files, and in the
/// worktree, tracked files that were deleted. Reports files searched to `watch` and stops when it's
/// cancelled.
pub fn grep(repo: &gix::Repository, args: GrepArgs, watch: &Watch) -> anyhow::Result<GrepResult> {
    let regex = build_regex(&args)?;
    let index = repo.index_or_empty()?;
    let mut spec = pathspec_search(repo, &args.pathspec, &index)?;
    let (rev, mut candidates) = candidates(repo, args.rev.as_deref(), &index)?;
    candidates.retain(|c| {
        spec.as_mut()
            .is_none_or(|s| s.is_included(c.path.as_bstr(), Some(false)))
    });

    let total = u64::try_from(candidates.len()).ok();
    let searched = watch.track(candidates.iter().map(Ok), total, "files searched");
    let matches = searched.flat_map(|candidate| {
        candidate
            .and_then(|candidate| search(repo, candidate, &regex))
            .map_or_else(
                |err| vec![Err(err)],
                |found| found.into_iter().map(Ok).collect(),
            )
    });
    let limit = page_limit(args.limit, DEFAULT_GREP_LIMIT, MAX_GREP_LIMIT);
    let (matches, truncated) = paginate(matches, args.skip.unwrap_or(0), limit)?;
    Ok(GrepResult {
        rev: rev.map(|id| id.to_string()),
        matches,
        truncated,
    })
}

fn build_regex(args: &GrepArgs) -> anyhow::Result<Regex> {
    let pattern = if args.fixed_strings {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .with_context(|| {
            format!(
                "`pattern` isn't a valid regex; fix it, or set `fixed_strings: true` to search for `{}` literally",
                args.pattern
            )
        })
}

/// The files to search in path order, and the object searched when `rev` names a revision.
fn candidates(
    repo: &gix::Repository,
    rev: Option<&str>,
    index: &gix::index::State,
) -> anyhow::Result<(Option<ObjectId>, Vec<Candidate>)> {
    let bare = repo.workdir().is_none();
    let rev = rev.unwrap_or(if bare { "HEAD" } else { "worktree" });
    match rev {
        "worktree" | "index" if bare => bail!(
            "this repository is bare, so it has no {rev}; pass a revision such as `HEAD` as `rev`"
        ),
        "worktree" => Ok((None, index_files(index, true))),
        "index" => Ok((None, index_files(index, false))),
        rev => {
            let id = resolve_object(repo, Some(rev))?;
            let tree = id.object()?.peel_to_tree().with_context(|| {
                format!("`{rev}` doesn't name a commit or tree, so there are no files to search")
            })?;
            let files = list_children(&tree, "".into(), true)?
                .into_iter()
                .filter(|entry| searchable(entry.kind))
                .map(|entry| Candidate {
                    path: entry.path,
                    content: Content::Blob(entry.id),
                })
                .collect();
            Ok((Some(id.detach()), files))
        }
    }
}

/// Regular and executable files in the index. From disk they're searched once per path even when
/// conflicted, as `git grep` does; from the index only the merged (stage 0) version is searched.
fn index_files(index: &gix::index::State, from_disk: bool) -> Vec<Candidate> {
    let mut files: Vec<Candidate> = index
        .entries()
        .iter()
        .filter(|entry| entry.mode == Mode::FILE || entry.mode == Mode::FILE_EXECUTABLE)
        .filter(|entry| from_disk || entry.stage() == Stage::Unconflicted)
        .map(|entry| Candidate {
            path: entry.path(index).to_owned(),
            content: if from_disk {
                Content::Disk
            } else {
                Content::Blob(entry.id)
            },
        })
        .collect();
    files.dedup_by(|a, b| a.path == b.path);
    files
}

fn searchable(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::Blob | EntryKind::BlobExecutable)
}

/// The matching lines of one file; empty for binary files and files gone from disk.
fn search(
    repo: &gix::Repository,
    candidate: &Candidate,
    regex: &Regex,
) -> anyhow::Result<Vec<GrepMatch>> {
    let Some(data) = load(repo, candidate)? else {
        return Ok(Vec::new());
    };
    if is_binary(&data) {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for (n, line) in data.lines_with_terminator().enumerate() {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let Some(m) = regex.find(line) else {
            continue;
        };
        let text = String::from_utf8_lossy(line);
        let (clipped, text_truncated) = clip_utf8(&text, MAX_LINE_BYTES);
        found.push(GrepMatch {
            path: candidate.path.to_string(),
            line: n + 1,
            column: m.start() + 1,
            text: clipped.to_owned(),
            text_truncated,
        });
    }
    Ok(found)
}

fn load(repo: &gix::Repository, candidate: &Candidate) -> anyhow::Result<Option<Vec<u8>>> {
    match candidate.content {
        Content::Blob(id) => Ok(Some(repo.find_blob(id)?.detach().data)),
        Content::Disk => read_tracked_file(repo, candidate.path.as_bstr()),
    }
}

/// A tracked file's content on disk, or `None` when it was deleted or is no longer a regular file.
fn read_tracked_file(repo: &gix::Repository, path: &BStr) -> anyhow::Result<Option<Vec<u8>>> {
    let workdir = repo
        .workdir()
        .context("this repository is bare and has no worktree")?;
    let Ok(full) = worktree_path(workdir, path) else {
        // The parent directory is gone or now leads outside the repository: nothing to search.
        return Ok(None);
    };
    match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.is_file() => Ok(Some(std::fs::read(&full)?)),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read `{path}` from the worktree")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        bare_repo, empty_repo, fixture_repo, git_output, git_stdout, open, run_git, write,
    };

    fn args(pattern: &str, rev: Option<&str>) -> GrepArgs {
        GrepArgs {
            pattern: pattern.into(),
            rev: rev.map(Into::into),
            pathspec: vec![],
            ignore_case: false,
            fixed_strings: false,
            limit: None,
            skip: None,
        }
    }

    /// `path:line:column:text` rows from `git grep`, the oracle for grep.
    fn git_grep(dir: &std::path::Path, extra: &[&str]) -> Vec<String> {
        let mut cmd = vec![
            "-c",
            "core.quotePath=false",
            "grep",
            "-n",
            "--column",
            "-I",
            "--full-name",
            "--no-color",
        ];
        cmd.extend_from_slice(extra);
        let out = git_output(dir, &cmd, b"");
        // git grep exits 1 when nothing matches.
        assert!(out.status.code().unwrap() <= 1, "git grep {extra:?} failed");
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Our matches in `git grep` output form; with a revision, git prefixes each row with `<rev>:`.
    fn rows(res: &GrepResult, prefix: &str) -> Vec<String> {
        res.matches
            .iter()
            .map(|m| format!("{prefix}{}:{}:{}:{}", m.path, m.line, m.column, m.text))
            .collect()
    }

    /// Several files, nested directories, a unicode path, a binary file, a symlink and an executable.
    fn grep_fixture() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "src/main.rs", "fn main() {\n    let one = 1;\n}\n");
        write(
            root,
            "src/util/mod.rs",
            "// TODO: one more\npub fn two() {}\n",
        );
        write(root, "src-extra.txt", "One upper\nno newline at end: one");
        write(root, "dïr/ünï.txt", "ünï one\n");
        write(root, "data.bin", b"one\x00binary\n");
        std::os::unix::fs::symlink("a.txt", root.join("link-one")).unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "grep fixture"]);
        let repo = open(root);
        (dir, repo)
    }

    #[test]
    fn worktree_matches_git_grep() {
        let (dir, repo) = grep_fixture();
        let root = dir.path();
        // Unstaged edits are searched; untracked files are not.
        write(root, "a.txt", "one\ntwo\nthree one\n");
        write(root, "untracked.txt", "one\n");
        let res = grep(&repo, args("one", None), &Watch::none()).unwrap();
        assert_eq!(rows(&res, ""), git_grep(root, &["-e", "one"]));
        assert_eq!(res.matches.len(), 6, "{:?}", rows(&res, ""));
        assert!(res.rev.is_none() && !res.truncated);
        assert!(
            res.matches
                .iter()
                .all(|m| m.path != "data.bin" && m.path != "link-one")
        );
    }

    #[test]
    fn revisions_and_index_match_git_grep() {
        let (dir, repo) = grep_fixture();
        let root = dir.path();
        write(root, "a.txt", "staged one\n");
        run_git(root, &["add", "a.txt"]);
        write(root, "a.txt", "worktree only\n");

        let head = grep(&repo, args("one", Some("HEAD")), &Watch::none()).unwrap();
        assert_eq!(rows(&head, "HEAD:"), git_grep(root, &["-e", "one", "HEAD"]));
        assert_eq!(
            head.rev.as_deref(),
            Some(git_stdout(root, &["rev-parse", "HEAD"]).trim())
        );
        let old = grep(&repo, args("t", Some("v1")), &Watch::none()).unwrap();
        assert_eq!(rows(&old, "v1:"), git_grep(root, &["-e", "t", "v1"]));

        let index = grep(&repo, args("one", Some("index")), &Watch::none()).unwrap();
        assert_eq!(rows(&index, ""), git_grep(root, &["--cached", "-e", "one"]));
        assert_eq!(index.matches[0].text, "staged one");
    }

    #[test]
    fn options_match_git_grep() {
        let (dir, repo) = grep_fixture();
        let root = dir.path();
        let mut a = args("ONE", None);
        a.ignore_case = true;
        assert_eq!(
            rows(&grep(&repo, a, &Watch::none()).unwrap(), ""),
            git_grep(root, &["-i", "-e", "ONE"])
        );

        let mut a = args("fn .*\\(", None);
        a.pathspec = vec!["src/".into()];
        assert_eq!(
            rows(&grep(&repo, a, &Watch::none()).unwrap(), ""),
            git_grep(root, &["-E", "-e", "fn .*\\(", "--", "src/"])
        );

        let mut a = args("one", None);
        a.pathspec = vec!["*.txt".into(), ":(exclude)dïr".into()];
        assert_eq!(
            rows(&grep(&repo, a, &Watch::none()).unwrap(), ""),
            git_grep(root, &["-e", "one", "--", "*.txt", ":(exclude)dïr"])
        );

        let mut a = args("{}", None);
        assert!(
            grep(&repo, args("{", None), &Watch::none()).is_err(),
            "unbalanced brace is not a valid regex"
        );
        a.fixed_strings = true;
        assert_eq!(
            rows(&grep(&repo, a, &Watch::none()).unwrap(), ""),
            git_grep(root, &["-F", "-e", "{}"])
        );
    }

    #[test]
    fn paginates_matches() {
        let (_dir, repo) = grep_fixture();
        let all = grep(&repo, args("one", None), &Watch::none())
            .unwrap()
            .matches;
        let mut a = args("one", None);
        a.limit = Some(2);
        a.skip = Some(1);
        let page = grep(&repo, a, &Watch::none()).unwrap();
        assert_eq!(page.matches.len(), 2);
        assert_eq!(page.matches[0].path, all[1].path);
        assert!(page.truncated);
        let mut a = args("one", None);
        a.skip = Some(all.len() - 1);
        let last = grep(&repo, a, &Watch::none()).unwrap();
        assert_eq!(last.matches.len(), 1);
        assert!(!last.truncated);
    }

    #[test]
    fn long_lines_are_clipped() {
        let (dir, _) = fixture_repo();
        write(
            dir.path(),
            "min.js",
            format!("{}needle{}\n", "é".repeat(300), "x".repeat(100)),
        );
        let repo = open(dir.path());
        run_git(dir.path(), &["add", "."]);
        let res = grep(&repo, args("needle", None), &Watch::none()).unwrap();
        let m = &res.matches[0];
        assert!(m.text_truncated);
        assert!(m.text.len() <= MAX_LINE_BYTES);
        assert_eq!(m.column, 601, "the column counts bytes, like git");
    }

    #[test]
    fn deleted_and_conflicted_files() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "one\ntheirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "one\nours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!crate::tools::testutil::try_git(
            root,
            &["merge", "-q", "feature"]
        ));
        let repo = open(root);
        let res = grep(&repo, args("one|ours|theirs", None), &Watch::none()).unwrap();
        assert_eq!(
            rows(&res, ""),
            git_grep(root, &["-E", "-e", "one|ours|theirs"])
        );

        std::fs::remove_file(root.join("a.txt")).unwrap();
        assert!(
            grep(&repo, args("one", None), &Watch::none())
                .unwrap()
                .matches
                .is_empty()
        );
    }

    #[test]
    fn bare_empty_and_bad_revisions() {
        let (_dir, bare) = bare_repo();
        let res = grep(&bare, args("two", None), &Watch::none()).unwrap();
        assert_eq!(res.matches.len(), 1, "bare repositories search HEAD");
        assert!(res.rev.is_some());
        let err = format!(
            "{:#}",
            grep(&bare, args("two", Some("worktree")), &Watch::none()).unwrap_err()
        );
        assert!(err.contains("bare"), "{err}");

        let (_dir, empty) = empty_repo();
        assert!(
            grep(&empty, args("x", None), &Watch::none())
                .unwrap()
                .matches
                .is_empty()
        );
        let err = format!(
            "{:#}",
            grep(&empty, args("x", Some("HEAD")), &Watch::none()).unwrap_err()
        );
        assert!(err.contains("no commits yet"), "{err}");

        let (_dir, repo) = fixture_repo();
        let err = format!(
            "{:#}",
            grep(&repo, args("x", Some("nope")), &Watch::none()).unwrap_err()
        );
        assert!(err.contains("cannot resolve `nope`"), "{err}");
        let err = format!(
            "{:#}",
            grep(&repo, args("x", Some("HEAD:a.txt")), &Watch::none()).unwrap_err()
        );
        assert!(err.contains("doesn't name a commit or tree"), "{err}");
        let err = format!(
            "{:#}",
            grep(&repo, args("(", None), &Watch::none()).unwrap_err()
        );
        assert!(err.contains("fixed_strings"), "{err}");
    }

    #[test]
    fn args_defaults() {
        let parsed: GrepArgs = serde_json::from_str(r#"{"pattern":"x"}"#).unwrap();
        assert!(parsed.rev.is_none() && parsed.pathspec.is_empty());
        assert!(!parsed.ignore_case && !parsed.fixed_strings);
    }

    #[test]
    fn grep_reports_files_searched_and_stops_when_cancelled() {
        let (dir, repo) = fixture_repo();
        write(dir.path(), "b.txt", "two\n");
        run_git(dir.path(), &["add", "b.txt"]);
        let (watch, _, seen) = crate::tools::testutil::recording_watch(std::time::Duration::ZERO);
        grep(&repo, args("two", None), &watch).unwrap();
        let tracked = git_stdout(dir.path(), &["ls-files"]).lines().count() as u64;
        let last = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!((last.done, last.total), (tracked, Some(tracked)));
        assert_eq!(
            last.message,
            format!("{tracked} of {tracked} files searched")
        );

        let watch = crate::tools::testutil::cancelled_watch();
        let err = grep(&repo, args("two", None), &watch).unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }
}
