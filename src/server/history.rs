//! Commit history.

use rmcp::{
    RoleServer,
    handler::server::wrapper::{Json, Parameters},
    service::RequestContext,
    tool, tool_router,
};

use super::GitServer;
use crate::tools;

#[tool_router(router = history_router, vis = "pub(super)")]
impl GitServer {
    /// Commit history starting from a revision (default HEAD), newest first.
    #[tool(annotations(title = "Log", read_only_hint = true))]
    async fn log(
        &self,
        Parameters(args): Parameters<tools::history::LogArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<tools::history::LogResult>, String> {
        self.run_watched(context, move |r, w| tools::history::log(r, args, w))
            .await
            .map(Json)
    }

    /// Full metadata and message of a single commit (any revspec: SHA, branch, `HEAD~2`, tag).
    #[tool(annotations(title = "Show commit", read_only_hint = true))]
    async fn show_commit(
        &self,
        Parameters(args): Parameters<tools::history::ShowCommitArgs>,
    ) -> Result<Json<tools::history::CommitDetail>, String> {
        self.repo
            .run(move |r| tools::history::show_commit(r, args))
            .await
            .map(Json)
    }

    /// Commits that changed a file or directory, newest first, with how each commit changed it
    /// (added, modified, deleted, renamed). `follow_renames: true` traces one file across renames like
    /// `git log --follow`. Paginated (`limit`, default 20).
    #[tool(annotations(title = "File history", read_only_hint = true))]
    async fn file_history(
        &self,
        Parameters(args): Parameters<tools::history::FileHistoryArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<tools::history::FileHistoryResult>, String> {
        self.run_watched(context, move |r, w| {
            tools::history::file_history(r, args, w)
        })
        .await
        .map(Json)
    }

    /// Find commits by message regex, author regex, committer date window (`since`/`until`) and
    /// changed path. Every given filter must match. Newest first, paginated (`limit`, default 20).
    #[tool(annotations(title = "Search commits", read_only_hint = true))]
    async fn search_commits(
        &self,
        Parameters(args): Parameters<tools::history::SearchCommitsArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<tools::history::SearchCommitsResult>, String> {
        self.run_watched(context, move |r, w| {
            tools::history::search_commits(r, args, w)
        })
        .await
        .map(Json)
    }

    /// Notes attached to a commit (default HEAD) or other object, like `git notes show`. Reads
    /// `refs/notes/commits` (or `core.notesRef`) unless `ref` names another notes ref.
    #[tool(annotations(title = "Notes", read_only_hint = true))]
    async fn notes(
        &self,
        Parameters(args): Parameters<tools::notes::NotesArgs>,
    ) -> Result<Json<tools::notes::NotesResult>, String> {
        self.repo
            .run(move |r| tools::notes::notes(r, args))
            .await
            .map(Json)
    }
}
