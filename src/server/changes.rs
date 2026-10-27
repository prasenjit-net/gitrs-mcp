//! Local changes: the working tree and index compared with commits.

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};

use super::GitServer;
use crate::tools;

#[tool_router(router = changes_router, vis = "pub(super)")]
impl GitServer {
    /// Working-tree status like `git status`: current branch, and staged, unstaged, untracked and
    /// conflicted paths. Start here to see what changed locally. Each list holds at most `limit`
    /// entries (default 500); `counts` always has the full totals.
    #[tool(annotations(title = "Status", read_only_hint = true))]
    async fn status(
        &self,
        Parameters(args): Parameters<tools::status::StatusArgs>,
    ) -> Result<Json<tools::status::StatusResult>, String> {
        self.repo
            .run(move |r| tools::status::status(r, args))
            .await
            .map(Json)
    }

    /// Changed files and lines, like `git diff`. By default compares the working tree with the index
    /// (unstaged changes); `staged: true` compares the index with HEAD or `from` (staged changes);
    /// `from` + `to` compares two revisions. `format` is `stat` (default, line counts), `name_status`,
    /// or `patch` (unified diffs within `max_bytes`, default 64 KiB). Files are paginated (`limit`, default 100).
    #[tool(annotations(title = "Diff", read_only_hint = true))]
    async fn diff(
        &self,
        Parameters(args): Parameters<tools::diff::DiffArgs>,
    ) -> Result<Json<tools::diff::DiffResult>, String> {
        self.repo
            .run(move |r| tools::diff::diff(r, args))
            .await
            .map(Json)
    }

    /// Unresolved conflicts in the index during a merge, rebase, cherry-pick or stash pop: each file's
    /// conflict kind and its base, ours and theirs versions (blob ids, and text with `include_content`).
    /// Read the worktree file with `read_file` to see the conflict markers. Paginated (`limit`, default 100).
    #[tool(annotations(title = "Conflicts", read_only_hint = true))]
    async fn conflicts(
        &self,
        Parameters(args): Parameters<tools::merge::ConflictsArgs>,
    ) -> Result<Json<tools::merge::ConflictsResult>, String> {
        self.repo
            .run(move |r| tools::merge::conflicts(r, args))
            .await
            .map(Json)
    }

    /// Preview merging `theirs` into `ours` (default HEAD) without touching the repository, like
    /// `git merge-tree --write-tree`: whether it's clean, up to date or a fast-forward, the merge bases,
    /// the resulting tree, and every file that would conflict with its base/ours/theirs versions. Use it
    /// to answer "is it safe to merge X into Y?".
    #[tool(annotations(title = "Merge preview", read_only_hint = true))]
    async fn merge_preview(
        &self,
        Parameters(args): Parameters<tools::merge::MergePreviewArgs>,
    ) -> Result<Json<tools::merge::MergePreviewResult>, String> {
        self.repo
            .run(move |r| tools::merge::merge_preview(r, args))
            .await
            .map(Json)
    }
}
