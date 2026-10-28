use std::collections::HashMap;

use anyhow::{Context, bail};
use gix::bstr::{BStr, ByteSlice};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Signature, page_limit,
    progress::{Progress, Watch},
    repo_file_path, resolve_commit,
};

const DEFAULT_BLAME_LIMIT: usize = 200;
const MAX_BLAME_LIMIT: usize = 2000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BlameArgs {
    /// File to blame, relative to the repository root.
    pub path: String,
    /// Revision to blame at (default `HEAD`). Uncommitted changes are not blamed.
    pub rev: Option<String>,
    /// Only blame lines `[start, end]`: 1-based and inclusive, e.g. `[10, 20]`. Default: the whole file.
    pub line_range: Option<[u32; 2]>,
    /// Maximum hunks to return (default 200, max 2000). To continue, call again with `line_range`
    /// starting after the last returned hunk.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlameHunk {
    /// First line of the hunk in the file at `rev` (1-based).
    pub start_line: u32,
    /// Number of consecutive lines in the hunk.
    pub lines: u32,
    /// Full SHA of the commit that last changed these lines.
    pub commit_id: String,
    /// Author of that commit.
    pub author: Signature,
    /// First line of that commit's message.
    pub summary: String,
    /// Path of the file in that commit; differs from the requested path if the file was renamed since.
    pub original_path: String,
    /// Line in `original_path` at that commit where the hunk starts (1-based).
    pub original_start_line: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BlameResult {
    /// The normalized path that was blamed.
    pub path: String,
    /// Full SHA of the commit the blame started from.
    pub rev: String,
    /// Number of lines in the file at `rev`.
    pub line_count: u32,
    /// Hunks in file order. Adjacent hunks can share a commit when its lines came from different places.
    pub hunks: Vec<BlameHunk>,
    /// True if hunks were cut off at `limit`.
    pub truncated: bool,
}

/// Which commit last touched each line of a file, following renames like `git blame`. Tells `watch`
/// when it starts; gix's blame can't be interrupted, so cancellation is only noticed before it starts
/// and once it returns.
pub fn blame(
    repo: &gix::Repository,
    args: BlameArgs,
    watch: &Watch,
) -> anyhow::Result<BlameResult> {
    let path = repo_file_path(&args.path)?;
    let rev_name = args.rev.as_deref().unwrap_or("HEAD");
    let commit = resolve_commit(repo, args.rev.as_deref())?;
    let line_count = file_line_count(&commit, path.as_ref(), rev_name)?;
    let ranges = match args.line_range {
        None => gix::blame::BlameRanges::WholeFile,
        Some([start, end]) => {
            if start == 0 || start > end || end > line_count {
                bail!(
                    "line_range [{start}, {end}] is invalid: `{path}` has {line_count} lines; pass 1-based [start, end] with start <= end <= {line_count}"
                );
            }
            gix::blame::BlameRanges::from_one_based_inclusive_range(start..=end)?
        }
    };
    let options = gix::repository::blame_file::Options {
        ranges,
        // git blame follows renames by default.
        rewrites: Some(Default::default()),
        ..Default::default()
    };
    watch.check()?;
    watch.report(Progress {
        done: 0,
        total: None,
        message: format!("blaming {path} ({line_count} lines)"),
    });
    let outcome = match repo.blame_file(path.as_ref(), commit.id, options) {
        Ok(outcome) => outcome,
        // gix's blame follows recorded parents, so it fails at a shallow clone's boundary, where git
        // would stop and attribute the remaining lines to the boundary commit.
        Err(err) if repo.is_shallow().unwrap_or(false) => bail!(
            "cannot blame `{path}` in a shallow clone: history before the fetched commits is missing ({err}); run `git fetch --unshallow`, then try again"
        ),
        Err(err) => return Err(err.into()),
    };
    watch.check()?;

    let limit = page_limit(args.limit, DEFAULT_BLAME_LIMIT, MAX_BLAME_LIMIT);
    let mut commits = HashMap::new();
    let hunks = outcome
        .entries
        .iter()
        .take(limit)
        .map(|entry| {
            let (author, summary) = commit_info(repo, &mut commits, entry.commit_id)?;
            Ok(BlameHunk {
                start_line: entry.start_in_blamed_file + 1,
                lines: entry.len.get(),
                commit_id: entry.commit_id.to_string(),
                author,
                summary,
                original_path: entry.source_file_name.as_ref().unwrap_or(&path).to_string(),
                original_start_line: entry.start_in_source_file + 1,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(BlameResult {
        path: path.to_string(),
        rev: commit.id.to_string(),
        line_count,
        truncated: outcome.entries.len() > hunks.len(),
        hunks,
    })
}

fn file_line_count(commit: &gix::Commit<'_>, path: &BStr, rev: &str) -> anyhow::Result<u32> {
    let entry = commit
        .tree()?
        .lookup_entry(path.split(|b| *b == b'/'))?
        .with_context(|| {
            format!("`{path}` does not exist at `{rev}`; check the path with list_tree")
        })?;
    if !entry.mode().is_blob_or_symlink() {
        bail!("`{path}` is not a file at `{rev}`; blame works on files only");
    }
    let blob = entry.object()?;
    Ok(u32::try_from(blob.data.lines().count())?)
}

/// Author and summary of `id`, looked up once per commit since many hunks share one.
fn commit_info(
    repo: &gix::Repository,
    cache: &mut HashMap<gix::ObjectId, (Signature, String)>,
    id: gix::ObjectId,
) -> anyhow::Result<(Signature, String)> {
    if let Some(info) = cache.get(&id) {
        return Ok(info.clone());
    }
    let commit = repo.find_commit(id)?;
    let info = (
        Signature::from_ref(commit.author()?)?,
        commit.message()?.summary().to_string(),
    );
    cache.insert(id, info.clone());
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        empty_repo, fixture_repo, git_stdout, open, run_git, shallow_clone, write,
    };

    fn args(path: &str) -> BlameArgs {
        BlameArgs {
            path: path.into(),
            rev: None,
            line_range: None,
            limit: None,
        }
    }

    /// Commit SHA per final line number, from `git blame --porcelain`.
    fn git_blame(dir: &std::path::Path, extra: &[&str]) -> Vec<(u32, String)> {
        let mut cmd = vec!["blame", "--porcelain"];
        cmd.extend_from_slice(extra);
        let out = git_stdout(dir, &cmd);
        let mut lines = Vec::new();
        for line in out.lines() {
            let mut f = line.split(' ');
            let (Some(sha), Some(_orig), Some(fin)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                lines.push((fin.parse().unwrap(), sha.to_string()));
            }
        }
        lines
    }

    fn per_line(res: &BlameResult) -> Vec<(u32, String)> {
        res.hunks
            .iter()
            .flat_map(|h| (h.start_line..h.start_line + h.lines).map(|l| (l, h.commit_id.clone())))
            .collect()
    }

    /// Three commits each owning part of `story.txt`, then a rename.
    fn blame_fixture() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "story.txt", "a\nb\nc\nd\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "story begins"]);
        write(root, "story.txt", "a\nB\nc\nd\ne\n");
        run_git(root, &["commit", "-q", "-am", "story edits"]);
        run_git(root, &["mv", "story.txt", "tale.txt"]);
        run_git(root, &["commit", "-q", "-m", "rename story"]);
        write(root, "tale.txt", "a\nB\nc\nd\ne\nf\n");
        run_git(root, &["commit", "-q", "-am", "tale continues"]);
        let repo = open(root);
        (dir, repo)
    }

    #[test]
    fn matches_git_blame_across_renames() {
        let (dir, repo) = blame_fixture();
        let res = blame(&repo, args("tale.txt"), &Watch::none()).unwrap();
        assert_eq!(per_line(&res), git_blame(dir.path(), &["tale.txt"]));
        assert_eq!(res.line_count, 6);
        assert!(!res.truncated);
        let first = &res.hunks[0];
        assert_eq!(first.summary, "story begins");
        assert_eq!(first.original_path, "story.txt");
        assert_eq!(first.author.email, "test@example.com");
        assert_eq!(res.hunks.last().unwrap().original_path, "tale.txt");
    }

    #[test]
    fn line_range_and_older_revisions() {
        let (dir, repo) = blame_fixture();
        let mut a = args("tale.txt");
        a.line_range = Some([2, 3]);
        let res = blame(&repo, a, &Watch::none()).unwrap();
        assert_eq!(
            per_line(&res),
            git_blame(dir.path(), &["-L", "2,3", "tale.txt"])
        );

        let mut a = args("story.txt");
        a.rev = Some("HEAD~2".into());
        let res = blame(&repo, a, &Watch::none()).unwrap();
        assert_eq!(
            per_line(&res),
            git_blame(dir.path(), &["HEAD~2", "--", "story.txt"])
        );
        assert_eq!(
            res.rev,
            git_stdout(dir.path(), &["rev-parse", "HEAD~2"]).trim()
        );
    }

    #[test]
    fn limit_truncates_hunks() {
        let (_dir, repo) = blame_fixture();
        let mut a = args("tale.txt");
        a.limit = Some(1);
        let res = blame(&repo, a, &Watch::none()).unwrap();
        assert_eq!(res.hunks.len(), 1);
        assert!(res.truncated);
    }

    #[test]
    fn shallow_clones_say_how_to_get_the_history() {
        let (source, _) = blame_fixture();
        let (_dir, repo) = shallow_clone(source.path());
        let err = format!(
            "{:#}",
            blame(&repo, args("tale.txt"), &Watch::none()).unwrap_err()
        );
        assert!(
            err.contains("shallow clone") && err.contains("git fetch --unshallow"),
            "{err}"
        );
    }

    #[test]
    fn errors_are_actionable() {
        let (_dir, repo) = blame_fixture();
        let msg = |a: BlameArgs| format!("{:#}", blame(&repo, a, &Watch::none()).unwrap_err());
        assert!(msg(args("missing.txt")).contains("does not exist"));
        let mut a = args("tale.txt");
        a.line_range = Some([5, 99]);
        assert!(msg(a).contains("has 6 lines"));
        let mut a = args("tale.txt");
        a.line_range = Some([0, 1]);
        assert!(msg(a).contains("1-based"));
        let mut a = args("tale.txt");
        a.rev = Some("nope".into());
        assert!(msg(a).contains("cannot resolve"));

        let (dir, _) = fixture_repo();
        write(dir.path(), "dir/x.txt", "x\n");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "dir"]);
        let err = format!(
            "{:#}",
            blame(&open(dir.path()), args("dir"), &Watch::none()).unwrap_err()
        );
        assert!(err.contains("not a file"), "{err}");

        let (_dir, empty) = empty_repo();
        assert!(blame(&empty, args("a.txt"), &Watch::none()).is_err());
    }

    #[test]
    fn blame_announces_itself_and_honours_cancellation() {
        let (_dir, repo) = fixture_repo();
        let (watch, _, seen) = crate::tools::testutil::recording_watch(std::time::Duration::ZERO);
        blame(&repo, args("a.txt"), &watch).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].message, "blaming a.txt (2 lines)");

        let watch = crate::tools::testutil::cancelled_watch();
        let err = blame(&repo, args("a.txt"), &watch).unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }
}
