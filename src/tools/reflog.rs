use anyhow::Context;
use gix::bstr::ByteSlice;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Signature, page_limit, paginate};

const DEFAULT_REFLOG_LIMIT: usize = 20;
const MAX_REFLOG_LIMIT: usize = 500;
/// git keeps stashes as the reflog of this ref; gix has no stash API, but reading the reflog is enough to list them.
const STASH_REF: &str = "refs/stash";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReflogArgs {
    /// Ref whose reflog to read (default `HEAD`): `HEAD`, a branch such as `main`, or a full name such
    /// as `refs/heads/main`. Short names resolve like `git rev-parse` does.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// Maximum entries to return (default 20, max 500).
    pub limit: Option<usize>,
    /// Number of entries to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReflogEntry {
    /// Revision naming this entry, such as `HEAD@{2}` or `main@{0}`. Pass it to other tools as `rev`.
    pub selector: String,
    /// Full SHA the ref pointed to before this update; `null` when the update created the ref.
    pub old_id: Option<String>,
    /// Full SHA the ref pointed to after this update.
    pub new_id: String,
    /// Who made the update, and when.
    pub committer: Signature,
    /// What git recorded for the update, e.g. `commit: fix typo` or `checkout: moving from main to feature`.
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReflogResult {
    /// Full name of the ref whose log was read, e.g. `refs/heads/main` or `HEAD`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Updates newest first, like `git reflog`. Empty when the ref has no reflog.
    pub entries: Vec<ReflogEntry>,
    /// True if older entries exist past this page.
    pub truncated: bool,
}

/// Where a ref pointed over time, newest first, like `git reflog show <ref>`.
pub fn reflog(repo: &gix::Repository, args: ReflogArgs) -> anyhow::Result<ReflogResult> {
    let name = args.reference.as_deref().unwrap_or("HEAD");
    let not_found = || {
        format!(
            "no ref named `{name}`; pass HEAD, a branch name, or a full ref name (list_refs shows what exists)"
        )
    };
    let reference = repo
        .try_find_reference(name)
        .with_context(not_found)?
        .with_context(not_found)?;
    let short = reference.name().shorten().to_string();
    let limit = page_limit(args.limit, DEFAULT_REFLOG_LIMIT, MAX_REFLOG_LIMIT);
    let (entries, truncated) =
        newest_first(&reference, args.skip.unwrap_or(0), limit, |n, line| {
            Ok(ReflogEntry {
                selector: format!("{short}@{{{n}}}"),
                old_id: (!line.previous_oid.is_null()).then(|| line.previous_oid.to_string()),
                new_id: line.new_oid.to_string(),
                committer: Signature::from_owned(&line.signature),
                message: line.message.to_string(),
            })
        })?;
    Ok(ReflogResult {
        reference: reference.name().as_bstr().to_string(),
        entries,
        truncated,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListStashesArgs {
    /// Maximum stashes to return (default 20, max 500).
    pub limit: Option<usize>,
    /// Number of stashes to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StashEntry {
    /// Position in the stash list; 0 is the most recent stash.
    pub index: usize,
    /// Revision naming the stash, e.g. `stash@{0}`. To see what it holds, call `diff` with `from` set to
    /// `base` and `to` set to this selector.
    pub selector: String,
    /// Full SHA of the stash commit, which records the stashed worktree state.
    pub id: String,
    /// Full SHA of the commit HEAD was on when the stash was made (the stash commit's first parent).
    pub base: String,
    /// Branch HEAD was on when stashing; `null` when HEAD was detached.
    pub branch: Option<String>,
    /// Stash message as `git stash list` shows it, e.g. `WIP on main: 1a2b3c4 subject` or `On main: my message`.
    pub message: String,
    /// True when the stash also holds untracked files (`git stash -u` or `-a`).
    pub includes_untracked: bool,
    /// Who made the stash, and when.
    pub committer: Signature,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListStashesResult {
    /// Stashes newest first, like `git stash list`. Empty when there are none.
    pub stashes: Vec<StashEntry>,
    /// True if older stashes exist past this page.
    pub truncated: bool,
}

/// The stash list, newest first, like `git stash list`.
pub fn list_stashes(
    repo: &gix::Repository,
    args: ListStashesArgs,
) -> anyhow::Result<ListStashesResult> {
    let Some(reference) = repo.try_find_reference(STASH_REF)? else {
        return Ok(ListStashesResult {
            stashes: Vec::new(),
            truncated: false,
        });
    };
    let limit = page_limit(args.limit, DEFAULT_REFLOG_LIMIT, MAX_REFLOG_LIMIT);
    let (stashes, truncated) =
        newest_first(&reference, args.skip.unwrap_or(0), limit, |index, line| {
            let commit = repo.find_commit(line.new_oid).with_context(|| {
                format!(
                    "stash@{{{index}}} points to {}, which is not a commit",
                    line.new_oid
                )
            })?;
            let parents: Vec<_> = commit.parent_ids().collect();
            Ok(StashEntry {
                index,
                selector: format!("stash@{{{index}}}"),
                id: line.new_oid.to_string(),
                base: parents
                    .first()
                    .with_context(|| format!("stash@{{{index}}} has no parent commit"))?
                    .to_string(),
                branch: stash_branch(line.message.as_bstr()),
                message: line.message.to_string(),
                // git stores untracked files as an optional third parent.
                includes_untracked: parents.len() > 2,
                committer: Signature::from_owned(&line.signature),
            })
        })?;
    Ok(ListStashesResult { stashes, truncated })
}

/// The branch named in a stash message: `WIP on <branch>: …` or `On <branch>: …`.
/// git writes `(no branch)` for a detached HEAD.
fn stash_branch(message: &gix::bstr::BStr) -> Option<String> {
    let rest = message
        .strip_prefix(b"WIP on ")
        .or_else(|| message.strip_prefix(b"On "))?;
    let branch = rest.split_str(": ").next()?.to_str().ok()?;
    (branch != "(no branch)").then(|| branch.to_owned())
}

/// One page of `reference`'s reflog, newest first, with each line's position (0 = newest).
fn newest_first<T>(
    reference: &gix::Reference<'_>,
    skip: usize,
    limit: usize,
    mut entry: impl FnMut(usize, gix::refs::log::Line) -> anyhow::Result<T>,
) -> anyhow::Result<(Vec<T>, bool)> {
    let mut platform = reference.log_iter();
    let Some(lines) = platform.rev()? else {
        return Ok((Vec::new(), false));
    };
    let entries = lines
        .enumerate()
        .map(|(n, line)| entry(n, line.map_err(anyhow::Error::from)?));
    paginate(entries, skip, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git, write,
    };

    fn reflog_of(repo: &gix::Repository, name: Option<&str>) -> anyhow::Result<ReflogResult> {
        reflog(
            repo,
            ReflogArgs {
                reference: name.map(Into::into),
                limit: None,
                skip: None,
            },
        )
    }

    /// `(selector, new id, message)` rows from `git log -g`, the oracle for reflog.
    fn git_reflog(dir: &std::path::Path, name: &str) -> Vec<(String, String, String)> {
        git_stdout(dir, &["log", "-g", "--format=%gd%x09%H%x09%gs", name])
            .lines()
            .map(|line| {
                let f: Vec<_> = line.splitn(3, '\t').collect();
                (f[0].to_owned(), f[1].to_owned(), f[2].to_owned())
            })
            .collect()
    }

    fn rows(res: &ReflogResult) -> Vec<(String, String, String)> {
        res.entries
            .iter()
            .map(|e| (e.selector.clone(), e.new_id.clone(), e.message.clone()))
            .collect()
    }

    #[test]
    fn head_and_branch_reflogs_match_git() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        run_git(root, &["checkout", "-q", "main"]);
        let repo = open(root);

        let head = reflog_of(&repo, None).unwrap();
        assert_eq!(head.reference, "HEAD");
        assert_eq!(rows(&head), git_reflog(root, "HEAD"));
        assert!(
            head.entries[0]
                .message
                .starts_with("checkout: moving from feature to main")
        );

        let main = reflog_of(&repo, Some("main")).unwrap();
        assert_eq!(main.reference, "refs/heads/main");
        assert_eq!(rows(&main), git_reflog(root, "main"));
        let first = main.entries.last().unwrap();
        assert!(first.old_id.is_none(), "the first entry creates the branch");
        assert_eq!(
            main.entries[0].old_id.as_deref(),
            Some(first.new_id.as_str())
        );
        assert_eq!(main.entries[0].committer.email, "test@example.com");
    }

    #[test]
    fn reflog_paginates() {
        let (dir, repo) = fixture_repo();
        let all = git_reflog(dir.path(), "HEAD");
        let page = reflog(
            &repo,
            ReflogArgs {
                reference: Some("HEAD".into()),
                limit: Some(1),
                skip: Some(1),
            },
        )
        .unwrap();
        assert_eq!(rows(&page), all[1..2]);
        assert_eq!(page.truncated, all.len() > 2);
        let past_end = reflog(
            &repo,
            ReflogArgs {
                reference: None,
                limit: None,
                skip: Some(all.len()),
            },
        )
        .unwrap();
        assert!(past_end.entries.is_empty() && !past_end.truncated);
    }

    #[test]
    fn reflog_edge_cases() {
        let (_dir, repo) = fixture_repo();
        let err = format!("{:#}", reflog_of(&repo, Some("nope")).unwrap_err());
        assert!(err.contains("no ref named `nope`"), "{err}");

        // A tag has no reflog by default.
        let tag = reflog_of(&repo, Some("v1")).unwrap();
        assert_eq!(tag.reference, "refs/tags/v1");
        assert!(tag.entries.is_empty());

        let (_dir, empty) = empty_repo();
        assert!(reflog_of(&empty, None).unwrap().entries.is_empty());
        let (_dir, bare) = bare_repo();
        assert!(reflog_of(&bare, Some("main")).is_ok());
    }

    #[test]
    fn stashes_match_git_stash_list() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "a.txt", "stashed edit\n");
        run_git(root, &["stash", "-q"]);
        write(root, "a.txt", "named edit\n");
        write(root, "new.txt", "untracked\n");
        run_git(root, &["stash", "push", "-q", "-u", "-m", "with untracked"]);
        run_git(root, &["checkout", "-q", "--detach"]);
        write(root, "a.txt", "detached edit\n");
        run_git(root, &["stash", "-q"]);
        let repo = open(root);

        let res = list_stashes(
            &repo,
            ListStashesArgs {
                limit: None,
                skip: None,
            },
        )
        .unwrap();
        let git: Vec<_> = git_stdout(root, &["stash", "list", "--format=%gd%x09%H%x09%gs"])
            .lines()
            .map(str::to_owned)
            .collect();
        let ours: Vec<_> = res
            .stashes
            .iter()
            .map(|s| format!("{}\t{}\t{}", s.selector, s.id, s.message))
            .collect();
        assert_eq!(ours, git);

        let head = git_stdout(root, &["rev-parse", "HEAD"]).trim().to_owned();
        assert_eq!(res.stashes[0].branch, None, "detached HEAD");
        assert_eq!(res.stashes[1].branch.as_deref(), Some("main"));
        assert!(res.stashes[1].includes_untracked);
        assert!(!res.stashes[2].includes_untracked);
        assert_eq!(res.stashes[2].base, head);
        assert_eq!(res.stashes[2].index, 2);
        assert!(!res.truncated);

        let page = list_stashes(
            &repo,
            ListStashesArgs {
                limit: Some(2),
                skip: Some(0),
            },
        )
        .unwrap();
        assert_eq!(page.stashes.len(), 2);
        assert!(page.truncated);
    }

    #[test]
    fn no_stashes() {
        let (_dir, repo) = fixture_repo();
        let res = list_stashes(
            &repo,
            ListStashesArgs {
                limit: None,
                skip: None,
            },
        )
        .unwrap();
        assert!(res.stashes.is_empty() && !res.truncated);
        let (_dir, empty) = empty_repo();
        assert!(
            list_stashes(
                &empty,
                ListStashesArgs {
                    limit: None,
                    skip: None
                }
            )
            .unwrap()
            .stashes
            .is_empty()
        );
    }

    #[test]
    fn stash_branch_parsing() {
        assert_eq!(
            stash_branch("WIP on main: abc subject".into()).as_deref(),
            Some("main")
        );
        assert_eq!(
            stash_branch("On feat/x: message".into()).as_deref(),
            Some("feat/x")
        );
        assert_eq!(stash_branch("WIP on (no branch): abc s".into()), None);
        assert_eq!(stash_branch("something else".into()), None);
    }

    #[test]
    fn args_use_ref_as_the_field_name() {
        let parsed: ReflogArgs = serde_json::from_str(r#"{"ref":"main"}"#).unwrap();
        assert_eq!(parsed.reference.as_deref(), Some("main"));
        let parsed: ReflogArgs = serde_json::from_str("{}").unwrap();
        assert!(parsed.reference.is_none() && parsed.limit.is_none());
        let _: ListStashesArgs = serde_json::from_str("{}").unwrap();
    }
}
