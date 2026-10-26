use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Signature;

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
pub struct LogEntry {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub author: Signature,
    pub parents: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LogResult {
    pub commits: Vec<LogEntry>,
    /// True if more commits exist past this page.
    pub truncated: bool,
}

pub fn log(repo: &gix::Repository, args: LogArgs) -> anyhow::Result<LogResult> {
    let rev = args.rev.as_deref().unwrap_or("HEAD");
    let tip = repo.rev_parse_single(rev)?.object()?.peel_to_commit()?.id;
    let limit = args
        .limit
        .unwrap_or(DEFAULT_LOG_LIMIT)
        .clamp(1, MAX_LOG_LIMIT);

    let mut walk = repo.rev_walk([tip]);
    if args.first_parent {
        walk = walk.first_parent_only();
    }
    let mut iter = walk.all()?.skip(args.skip.unwrap_or(0));

    let mut commits = Vec::with_capacity(limit);
    for info in iter.by_ref().take(limit) {
        let commit = info?.object()?;
        commits.push(LogEntry {
            id: commit.id.to_string(),
            short_id: commit.short_id()?.to_string(),
            summary: commit.message()?.summary().to_string(),
            author: Signature::from_ref(commit.author()?)?,
            parents: commit.parent_ids().map(|p| p.to_string()).collect(),
        });
    }
    Ok(LogResult {
        truncated: iter.next().is_some(),
        commits,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShowCommitArgs {
    /// Commit to show (default `HEAD`).
    pub rev: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitDetail {
    pub id: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author: Signature,
    pub committer: Signature,
    pub summary: String,
    /// Full raw message including the summary line.
    pub message: String,
}

pub fn show_commit(repo: &gix::Repository, args: ShowCommitArgs) -> anyhow::Result<CommitDetail> {
    let rev = args.rev.as_deref().unwrap_or("HEAD");
    let commit = repo.rev_parse_single(rev)?.object()?.peel_to_commit()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::fixture_repo;

    fn log_args(limit: Option<usize>, skip: Option<usize>) -> LogArgs {
        LogArgs {
            rev: None,
            limit,
            skip,
            first_parent: false,
        }
    }

    #[test]
    fn log_is_newest_first() {
        let (_dir, repo) = fixture_repo();
        let res = log(&repo, log_args(None, None)).unwrap();
        let summaries: Vec<_> = res.commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(summaries, ["second commit", "first commit"]);
        assert!(!res.truncated);
        assert_eq!(res.commits[0].author.email, "test@example.com");
    }

    #[test]
    fn log_paginates() {
        let (_dir, repo) = fixture_repo();
        let page = log(&repo, log_args(Some(1), None)).unwrap();
        assert_eq!(page.commits.len(), 1);
        assert!(page.truncated);
        let next = log(&repo, log_args(Some(1), Some(1))).unwrap();
        assert_eq!(next.commits[0].summary, "first commit");
        assert!(!next.truncated);
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
}
