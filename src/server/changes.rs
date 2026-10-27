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
}
