//! MCP prompts: ready-made requests that come filled in with the repository data they need, such as
//! the staged diff for a commit message.

use anyhow::{Context, bail};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    ChangeKind, DEFAULT_MAX_BYTES, Signature,
    blame::{BlameArgs, blame},
    diff::{DiffFile, DiffResult, diff},
    history::{FileHistoryArgs, LogArgs, ShowCommitArgs, file_history, log, show_commit},
    progress::Watch,
    resources::{diff_args, patch_args, render},
};

/// Patch text a prompt includes, in bytes; past it, the prompt says what was left out.
const PROMPT_PATCH_BYTES: usize = 2 * DEFAULT_MAX_BYTES;
/// Recent commit subjects shown as a style guide for a commit message.
const STYLE_EXAMPLES: usize = 10;
/// Files and hunks per file whose old lines `review_changes` blames.
const BLAMED_FILES: usize = 20;
const BLAMED_HUNKS: usize = 10;
const DEFAULT_HISTORY: usize = 20;
const MAX_HISTORY: usize = 100;
/// Commits whose patch `explain_history` includes, newest first.
const DETAILED_COMMITS: usize = 10;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ReviewArgs {
    /// Old side of the changes to review: a revision, or `index` (default `HEAD`).
    pub from: Option<String>,
    /// New side: a revision, `index` (default: the staged changes) or `worktree` (after `from: index`,
    /// the unstaged changes).
    pub to: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryArgs {
    /// File to explain, relative to the repository root, e.g. `src/main.rs`. Renames are followed.
    pub path: String,
    /// How many commits to include, newest first (default 20, max 100).
    pub limit: Option<String>,
}

/// Ask for a commit message for the staged changes, in the style of the repository's recent commits.
pub fn write_commit_message(repo: &gix::Repository) -> anyhow::Result<String> {
    let staged = diff(repo, diff_args("HEAD", "index", PROMPT_PATCH_BYTES)?)?;
    if staged.files.is_empty() {
        bail!("nothing is staged; stage changes with the stage tool (or `git add`) first");
    }
    let mut text = String::from(
        "Write a git commit message for the staged changes below. Start with a summary line of about \
         50 characters in the imperative mood, then a blank line and a body, wrapped at 72 columns, \
         that explains what changed and why. Reply with the message only.\n",
    );
    let subjects = recent_subjects(repo)?;
    if !subjects.is_empty() {
        text.push_str("\nMatch the style of this repository's recent commit subjects:\n\n");
        for subject in subjects {
            text.push_str(&format!("- {subject}\n"));
        }
    }
    text.push_str("\nStaged changes (`git diff --cached`):\n\n");
    text.push_str(&fenced("diff", &render(&staged)));
    Ok(text)
}

/// Subjects of the latest commits; none before the first commit.
fn recent_subjects(repo: &gix::Repository) -> anyhow::Result<Vec<String>> {
    if repo.head_id().is_err() {
        return Ok(Vec::new());
    }
    let args = LogArgs {
        rev: None,
        limit: Some(STYLE_EXAMPLES),
        skip: None,
        first_parent: true,
    };
    Ok(log(repo, args, &Watch::none())?
        .commits
        .into_iter()
        .map(|c| c.summary)
        .collect())
}

/// Ask for a code review of a diff, with the blame of the lines each hunk replaces, so the reviewer
/// can see which commits (and intentions) the change touches.
pub fn review_changes(repo: &gix::Repository, args: ReviewArgs) -> anyhow::Result<String> {
    let from = args.from.as_deref().unwrap_or("HEAD");
    let to = args.to.as_deref().unwrap_or("index");
    let changes = diff(repo, diff_args(from, to, PROMPT_PATCH_BYTES)?)?;
    if changes.files.is_empty() && changes.unmerged.is_empty() {
        bail!("there are no changes between `{from}` and `{to}` to review; pick other `from`/`to`");
    }
    let mut text = format!(
        "Review the changes below (`{from}..{to}`) as an experienced reviewer would. Point out bugs, \
         unhandled edge cases, security problems, and code that is hard to read or maintain, citing \
         file and line. Then say briefly what is good about the change. The blame shows which \
         commits last touched the lines each hunk replaces; use their summaries to judge whether \
         the change respects the original intent.\n\nDiff:\n\n"
    );
    text.push_str(&fenced("diff", &render(&changes)));
    // The old side of `index..worktree` is the index, which can't be blamed; HEAD is the closest commit.
    let blame_at = if from == "index" { "HEAD" } else { from };
    let notes = blame_notes(repo, &changes, blame_at)?;
    if !notes.is_empty() {
        text.push_str(&format!(
            "\nBlame of the replaced lines, at `{blame_at}`:\n\n{notes}"
        ));
    }
    Ok(text)
}

/// One line per blamed range of the old side of each modified file's hunks.
fn blame_notes(repo: &gix::Repository, changes: &DiffResult, rev: &str) -> anyhow::Result<String> {
    let mut notes = String::new();
    let blamable = changes
        .files
        .iter()
        .filter(|f| matches!(f.status, ChangeKind::Modified | ChangeKind::Renamed) && !f.binary);
    for file in blamable.take(BLAMED_FILES) {
        let path = file.old_path.as_deref().unwrap_or(&file.path);
        for [start, end] in old_ranges(file).into_iter().take(BLAMED_HUNKS) {
            let args = BlameArgs {
                path: path.to_owned(),
                rev: Some(rev.to_owned()),
                line_range: Some([start, end]),
                limit: None,
            };
            // A path the diff saw may not exist at `rev` (staged on top of another commit); skip it.
            let Ok(found) = blame(repo, args, &Watch::none()) else {
                continue;
            };
            for hunk in found.hunks {
                let last = hunk.start_line + hunk.lines - 1;
                notes.push_str(&format!(
                    "- {path} lines {}-{last}: {} {}\n",
                    hunk.start_line,
                    &hunk.commit_id[..hunk.commit_id.len().min(12)],
                    describe(&hunk.summary, &hunk.author)
                ));
            }
        }
    }
    Ok(notes)
}

/// The old-side line range `[start, end]` of each hunk in a file's patch, from `@@ -start,len` headers.
/// Pure additions (`len` 0) replace nothing and are skipped.
fn old_ranges(file: &DiffFile) -> Vec<[u32; 2]> {
    let Some(patch) = &file.patch else {
        return Vec::new();
    };
    patch
        .lines()
        .filter_map(|line| line.strip_prefix("@@ -"))
        .filter_map(|rest| {
            let range = rest.split(' ').next()?;
            let (start, len) = match range.split_once(',') {
                Some((start, len)) => (start.parse::<u32>().ok()?, len.parse::<u32>().ok()?),
                None => (range.parse::<u32>().ok()?, 1),
            };
            (len > 0 && start > 0).then(|| [start, start + len - 1])
        })
        .collect()
}

/// Ask for an explanation of how a file evolved, from its history and recent patches.
pub fn explain_history(repo: &gix::Repository, args: HistoryArgs) -> anyhow::Result<String> {
    let limit = match args.limit.as_deref() {
        None | Some("") => DEFAULT_HISTORY,
        Some(text) => text
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .with_context(|| format!("`limit` must be a positive number, not `{text}`"))?
            .min(MAX_HISTORY),
    };
    let history = file_history(
        repo,
        FileHistoryArgs {
            path: args.path.clone(),
            rev: None,
            limit: Some(limit),
            skip: None,
            follow_renames: true,
        },
        &Watch::none(),
    )?;
    if history.commits.is_empty() {
        bail!(
            "`{}` has no history at HEAD; check the path with list_tree",
            args.path
        );
    }
    let mut text = format!(
        "Explain how `{}` evolved and why, using its history below: the main phases of its \
         development, the reasons behind significant changes (from the commit messages), and \
         anything a newcomer should know before changing it.\n\nCommits that changed it, newest \
         first:\n\n",
        args.path
    );
    for entry in &history.commits {
        let renamed = entry
            .old_path
            .as_ref()
            .map(|old| format!(" (from {old})"))
            .unwrap_or_default();
        text.push_str(&format!(
            "- {} {}{renamed}: {}\n",
            entry.commit.short_id,
            format!("{:?}", entry.change).to_lowercase(),
            describe(&entry.commit.summary, &entry.commit.author)
        ));
    }
    if history.truncated {
        text.push_str(&format!(
            "- (older commits left out; pass a larger `limit`, up to {MAX_HISTORY})\n"
        ));
    }
    text.push_str("\nThe most recent changes in detail:\n");
    let mut budget = PROMPT_PATCH_BYTES;
    for entry in history.commits.iter().take(DETAILED_COMMITS) {
        let detail = show_commit(
            repo,
            ShowCommitArgs {
                rev: Some(entry.commit.id.clone()),
            },
        )?;
        let from = match detail.parents.first() {
            Some(parent) => parent.clone(),
            None => repo.empty_tree().id.to_string(),
        };
        let mut args = patch_args(Some(from), Some(detail.id), false, budget);
        args.pathspec = std::iter::once(entry.path.clone())
            .chain(entry.old_path.clone())
            .map(|p| format!(":(literal){p}"))
            .collect();
        let patch = render(&diff(repo, args)?);
        text.push_str(&format!(
            "\n### {} by {}, {}\n\n{}\n\n{}",
            entry.commit.short_id,
            entry.commit.author.name,
            short_date(&entry.commit.author),
            detail.message.trim_end(),
            fenced("diff", &patch)
        ));
        budget = budget.saturating_sub(patch.len());
        if budget == 0 {
            text.push_str("\n(Older patches left out to keep the prompt short.)\n");
            break;
        }
    }
    Ok(text)
}

/// `"summary" by Name, YYYY-MM-DD`.
fn describe(summary: &str, author: &Signature) -> String {
    format!("\"{summary}\" by {}, {}", author.name, short_date(author))
}

/// The author date as `YYYY-MM-DD`, in the author's time zone.
fn short_date(author: &Signature) -> String {
    gix::date::Time::new(author.time, author.offset).format_or_unix(gix::date::time::format::SHORT)
}

/// `text` in a Markdown code fence longer than any backtick run inside it, so it can't close early.
fn fenced(language: &str, text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let body = if text.ends_with('\n') || text.is_empty() {
        text.to_owned()
    } else {
        format!("{text}\n")
    };
    format!("{fence}{language}\n{body}{fence}\n")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{empty_repo, fixture_repo, git_stdout, run_git, write};

    fn git_diff(root: &Path, args: &[&str]) -> String {
        git_stdout(
            root,
            &[&["-c", "core.quotePath=false", "diff"], args].concat(),
        )
    }

    /// Short ids of the commits `git blame` names for lines `start..=end` of `path` at `rev`.
    fn git_blame(root: &Path, rev: &str, path: &str, start: u32, end: u32) -> Vec<String> {
        let range = format!("{start},{end}");
        // Full ids from the porcelain headers: `--abbrev` widths differ between git versions.
        git_stdout(
            root,
            &["blame", "--porcelain", "-L", &range, rev, "--", path],
        )
        .lines()
        .filter_map(|line| line.split(' ').next())
        .filter(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|id| id[..12].to_owned())
        .collect()
    }

    #[test]
    fn commit_message_prompt_has_the_staged_diff_and_style() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let err = write_commit_message(&repo).unwrap_err().to_string();
        assert!(err.contains("nothing is staged"), "{err}");

        write(root, "a.txt", "one\ntwo\nthree\n");
        write(root, "unstaged.txt", "not part of it\n");
        run_git(root, &["add", "a.txt"]);
        let text = write_commit_message(&repo).unwrap();
        assert!(text.contains(&fenced("diff", &git_diff(root, &["--cached"]))));
        let subjects = git_stdout(root, &["log", "--first-parent", "-10", "--format=- %s"]);
        assert!(text.contains(&subjects), "{text}");
        assert!(!text.contains("unstaged.txt"));
    }

    #[test]
    fn commit_message_prompt_before_the_first_commit() {
        let (dir, repo) = empty_repo();
        write(dir.path(), "first.txt", "hello\n");
        run_git(dir.path(), &["add", "first.txt"]);
        let text = write_commit_message(&repo).unwrap();
        assert!(text.contains("+++ b/first.txt"));
        assert!(!text.contains("recent commit subjects"));
    }

    /// A file whose lines come from three commits, then a change touching two of them.
    fn layered(root: &Path) {
        write(root, "f.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
        run_git(root, &["add", "f.txt"]);
        run_git(root, &["commit", "-q", "-m", "add f"]);
        write(root, "f.txt", "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\n");
        run_git(root, &["commit", "-q", "-am", "capitalize b"]);
        write(root, "f.txt", "a\nB\nc\nd\ne\nf\ng\nh\nI\nj\n");
        run_git(root, &["commit", "-q", "-am", "capitalize i"]);
    }

    #[test]
    fn review_prompt_blames_the_replaced_lines() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        layered(root);
        write(root, "f.txt", "a\nB!\nc\nd\ne\nf\ng\nh\nI!\nj\n");
        run_git(root, &["add", "f.txt"]);
        write(root, "new.txt", "added files have nothing to blame\n");
        run_git(root, &["add", "new.txt"]);

        let text = review_changes(&repo, ReviewArgs::default()).unwrap();
        assert!(text.contains(&fenced("diff", &git_diff(root, &["--cached"]))));
        // Hunks with three lines of context: old lines 1-5 and 6-10.
        let mut expected = git_blame(root, "HEAD", "f.txt", 1, 5);
        expected.extend(git_blame(root, "HEAD", "f.txt", 6, 10));
        expected.dedup();
        let named: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("- f.txt lines "))
            .map(|l| l.split(' ').nth(1).unwrap().to_owned())
            .collect();
        assert_eq!(named, expected);
        assert!(text.contains("\"capitalize b\" by Test"));
        assert!(!text.contains("- new.txt"));
    }

    #[test]
    fn review_prompt_sides_and_refusals() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        layered(root);
        let err = review_changes(&repo, ReviewArgs::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no changes between `HEAD` and `index`"),
            "{err}"
        );

        write(root, "f.txt", "A\nB\nc\nd\ne\nf\ng\nh\nI\nj\n");
        let unstaged = ReviewArgs {
            from: Some("index".into()),
            to: Some("worktree".into()),
        };
        let text = review_changes(&repo, unstaged).unwrap();
        assert!(text.contains(&fenced("diff", &git_diff(root, &[]))));
        assert!(
            text.contains("at `HEAD`"),
            "the index can't be blamed, HEAD stands in"
        );

        let commits = ReviewArgs {
            from: Some("HEAD~2".into()),
            to: Some("HEAD".into()),
        };
        let text = review_changes(&repo, commits).unwrap();
        assert!(text.contains(&fenced("diff", &git_diff(root, &["HEAD~2", "HEAD"]))));
        assert!(text.contains("at `HEAD~2`"));

        let bad = ReviewArgs {
            from: Some("HEAD".into()),
            to: Some("worktree".into()),
        };
        assert!(
            review_changes(&repo, bad)
                .unwrap_err()
                .to_string()
                .contains("index..worktree")
        );
    }

    #[test]
    fn renamed_files_blame_their_old_path() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        layered(root);
        run_git(root, &["mv", "f.txt", "g.txt"]);
        write(root, "g.txt", "a\nB\nc\nd\ne\nf\ng\nh\nI\nJ\n");
        run_git(root, &["add", "g.txt"]);
        let text = review_changes(&repo, ReviewArgs::default()).unwrap();
        assert!(text.contains("rename from f.txt"));
        assert!(text.contains("- f.txt lines 7-"), "{text}");
    }

    fn patch_of(text: &str) -> DiffFile {
        DiffFile {
            path: "x".into(),
            old_path: None,
            status: ChangeKind::Modified,
            additions: None,
            deletions: None,
            binary: false,
            patch: Some(text.into()),
            patch_truncated: false,
        }
    }

    #[test]
    fn hunk_headers_give_old_line_ranges() {
        let patch =
            "diff --git a/x b/x\n@@ -3,4 +3,5 @@ fn f\n ctx\n@@ -9 +10 @@\n-x\n@@ -0,0 +1 @@\n+y\n";
        assert_eq!(old_ranges(&patch_of(patch)), [[3, 6], [9, 9]]);
        let mut binary = patch_of("");
        binary.patch = None;
        assert!(old_ranges(&binary).is_empty());
    }

    fn history_args(path: &str, limit: Option<&str>) -> HistoryArgs {
        HistoryArgs {
            path: path.into(),
            limit: limit.map(str::to_owned),
        }
    }

    #[test]
    fn history_prompt_follows_the_file() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        layered(root);
        run_git(root, &["mv", "f.txt", "g.txt"]);
        run_git(
            root,
            &[
                "commit",
                "-q",
                "-m",
                "rename f to g\n\nbecause g is a better name",
            ],
        );

        let text = explain_history(&repo, history_args("g.txt", None)).unwrap();
        let ids = git_stdout(root, &["log", "--follow", "--format=%h", "--", "g.txt"]);
        let listed: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("- "))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(listed, ids.lines().collect::<Vec<_>>());
        assert!(text.contains(" renamed (from f.txt): \"rename f to g\""));
        assert!(text.contains("because g is a better name"));
        let first = git_diff(root, &["HEAD~2", "HEAD~1", "--", "f.txt"]);
        assert!(text.contains(&fenced("diff", &first)), "{text}");

        let short = explain_history(&repo, history_args("g.txt", Some(" 2 "))).unwrap();
        assert!(short.contains("older commits left out"));
        assert_eq!(short.matches("\n### ").count(), 2);
        explain_history(&repo, history_args("g.txt", Some(""))).unwrap();
        explain_history(&repo, history_args("g.txt", Some("100000"))).unwrap();
    }

    #[test]
    fn history_prompt_refusals_and_budget() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        for bad in ["0", "-1", "x"] {
            let err = explain_history(&repo, history_args("a.txt", Some(bad))).unwrap_err();
            assert!(err.to_string().contains("positive number"), "{bad}");
        }
        assert!(explain_history(&repo, history_args("missing.txt", None)).is_err());

        let line = "0123456789".repeat(10);
        for round in 0..3 {
            let body: String = (0..1500).map(|i| format!("{round} {i} {line}\n")).collect();
            write(root, "big.txt", body);
            run_git(root, &["add", "big.txt"]);
            run_git(root, &["commit", "-q", "-m", &format!("big {round}")]);
        }
        let text = explain_history(&repo, history_args("big.txt", None)).unwrap();
        assert!(
            text.contains("Older patches left out"),
            "the budget stops the details"
        );
    }

    #[test]
    fn fences_outlast_backticks_inside() {
        assert_eq!(fenced("diff", "a\n"), "```diff\na\n```\n");
        assert_eq!(fenced("", "no newline"), "```\nno newline\n```\n");
        assert_eq!(fenced("md", "````\n"), "`````md\n````\n`````\n");
    }
}
