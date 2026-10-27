use schemars::JsonSchema;
use serde::Serialize;

use super::in_progress;

#[derive(Debug, Serialize, JsonSchema)]
pub struct RepoInfo {
    /// Absolute path of the `.git` directory (the repository itself when bare).
    pub git_dir: String,
    /// `null` for bare repositories.
    pub workdir: Option<String>,
    /// True for a repository without a working tree.
    pub is_bare: bool,
    /// True for a shallow clone, whose history is cut off.
    pub is_shallow: bool,
    /// Short branch name HEAD points to; `null` when detached.
    pub head_branch: Option<String>,
    /// Commit HEAD resolves to; `null` in a fresh repo with no commits.
    pub head_commit: Option<String>,
    /// In-progress operation such as `Merge` or `Rebase`, if any.
    pub in_progress: Option<String>,
}

pub fn repo_info(repo: &gix::Repository) -> anyhow::Result<RepoInfo> {
    Ok(RepoInfo {
        git_dir: repo.git_dir().display().to_string(),
        workdir: repo.workdir().map(|p| p.display().to_string()),
        is_bare: repo.is_bare(),
        is_shallow: repo.is_shallow()?,
        head_branch: repo.head_name()?.map(|n| n.shorten().to_string()),
        head_commit: repo.head_id().ok().map(|id| id.to_string()),
        in_progress: in_progress(repo),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::fixture_repo;

    #[test]
    fn reports_head_and_branch() {
        let (_dir, repo) = fixture_repo();
        let info = repo_info(&repo).unwrap();
        assert_eq!(info.head_branch.as_deref(), Some("main"));
        assert!(info.head_commit.is_some());
        assert!(!info.is_bare);
        assert!(info.in_progress.is_none());
    }
}
