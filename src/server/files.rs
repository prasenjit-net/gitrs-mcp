//! File contents and line history.

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};

use super::GitServer;
use crate::tools;

#[tool_router(router = files_router, vis = "pub(super)")]
impl GitServer {
    /// Read a file at a revision (default HEAD), from the index (`rev: "index"`) or from the working
    /// tree (`rev: "worktree"`). Text is returned as UTF-8 and binary content as base64. At most
    /// `max_bytes` (default 64 KiB) per call; continue a truncated read with `offset_bytes: next_offset`.
    #[tool(annotations(title = "Read file", read_only_hint = true))]
    async fn read_file(
        &self,
        Parameters(args): Parameters<tools::files::ReadFileArgs>,
    ) -> Result<Json<tools::files::ReadFileResult>, String> {
        self.repo
            .run(move |r| tools::files::read_file(r, args))
            .await
            .map(Json)
    }

    /// List the files and directories of a revision's tree (default HEAD), like `git ls-tree`.
    /// Shows one directory level by default; `recursive: true` lists every file below `path`.
    /// Paginated (`limit`, default 500).
    #[tool(annotations(title = "List tree", read_only_hint = true))]
    async fn list_tree(
        &self,
        Parameters(args): Parameters<tools::files::ListTreeArgs>,
    ) -> Result<Json<tools::files::ListTreeResult>, String> {
        self.repo
            .run(move |r| tools::files::list_tree(r, args))
            .await
            .map(Json)
    }

    /// Which commit last changed each line of a file at a revision (default HEAD), following renames
    /// like `git blame`. Returns hunks of consecutive lines with commit, author and summary. Narrow
    /// big files with `line_range`; at most `limit` hunks (default 200) per call.
    #[tool(annotations(title = "Blame", read_only_hint = true))]
    async fn blame(
        &self,
        Parameters(args): Parameters<tools::blame::BlameArgs>,
    ) -> Result<Json<tools::blame::BlameResult>, String> {
        self.repo
            .run(move |r| tools::blame::blame(r, args))
            .await
            .map(Json)
    }
}
