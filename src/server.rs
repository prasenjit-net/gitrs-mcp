//! MCP surface: tool registration and server metadata.
//!
//! Each tool here is a thin async wrapper; the git logic lives in `crate::tools`
//! as plain sync functions over `&gix::Repository` so it can be tested directly.

use rmcp::{
    ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};

use crate::{repo::RepoHandle, tools};

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Gate for every tool that mutates the repo, index, or worktree.
    #[allow(dead_code)] // consulted once Phase 3 write tools land
    pub allow_write: bool,
}

#[derive(Clone)]
pub struct GitServer {
    repo: RepoHandle,
    #[allow(dead_code)]
    policy: Policy,
    tool_router: ToolRouter<Self>,
}

impl GitServer {
    pub fn new(repo: RepoHandle, policy: Policy) -> Self {
        Self {
            repo,
            policy,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl GitServer {
    /// Repository overview: paths, HEAD, current branch, bareness, shallowness,
    /// and any in-progress operation (merge, rebase, cherry-pick, ...).
    #[tool(annotations(title = "Repository info", read_only_hint = true))]
    async fn repo_info(&self) -> Result<Json<tools::repo::RepoInfo>, String> {
        self.repo.run(tools::repo::repo_info).await.map(Json)
    }

    /// Commit history starting from a revision (default HEAD), newest first.
    #[tool(annotations(title = "Log", read_only_hint = true))]
    async fn log(
        &self,
        Parameters(args): Parameters<tools::history::LogArgs>,
    ) -> Result<Json<tools::history::LogResult>, String> {
        self.repo
            .run(move |r| tools::history::log(r, args))
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

    /// List branches and/or tags with the commit each points to.
    #[tool(annotations(title = "List refs", read_only_hint = true))]
    async fn list_refs(
        &self,
        Parameters(args): Parameters<tools::refs::ListRefsArgs>,
    ) -> Result<Json<tools::refs::ListRefsResult>, String> {
        self.repo
            .run(move |r| tools::refs::list_refs(r, args))
            .await
            .map(Json)
    }

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

    /// Resolve a revision expression to full object SHAs, like `git rev-parse`: branches, tags, SHA
    /// prefixes, `HEAD~2`, `rev:path`, and ranges such as `a..b` and `a...b`. Use it to check that a
    /// name exists or to pin a moving ref to a SHA.
    #[tool(annotations(title = "Rev-parse", read_only_hint = true))]
    async fn rev_parse(
        &self,
        Parameters(args): Parameters<tools::revision::RevParseArgs>,
    ) -> Result<Json<tools::revision::RevParseResult>, String> {
        self.repo
            .run(move |r| tools::revision::rev_parse(r, args))
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
    ) -> Result<Json<tools::history::FileHistoryResult>, String> {
        self.repo
            .run(move |r| tools::history::file_history(r, args))
            .await
            .map(Json)
    }

    /// Find commits by message regex, author regex, committer date window (`since`/`until`) and
    /// changed path. Every given filter must match. Newest first, paginated (`limit`, default 20).
    #[tool(annotations(title = "Search commits", read_only_hint = true))]
    async fn search_commits(
        &self,
        Parameters(args): Parameters<tools::history::SearchCommitsArgs>,
    ) -> Result<Json<tools::history::SearchCommitsResult>, String> {
        self.repo
            .run(move |r| tools::history::search_commits(r, args))
            .await
            .map(Json)
    }

    /// How two revisions have diverged: their merge base, how many commits `head` is ahead of and
    /// behind `base`, and the newest of those commits (`limit`, default 20, per side). Use it before
    /// merging, or to check whether a branch is up to date.
    #[tool(annotations(title = "Compare refs", read_only_hint = true))]
    async fn compare_refs(
        &self,
        Parameters(args): Parameters<tools::revision::CompareRefsArgs>,
    ) -> Result<Json<tools::revision::CompareRefsResult>, String> {
        self.repo
            .run(move |r| tools::revision::compare_refs(r, args))
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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Local-only Git tools for a single repository. All output is structured JSON. \
                 Revisions accept anything `git rev-parse` would (SHA prefixes, branches, tags, HEAD~n). \
                 Paths and pathspecs are relative to the repository root. Lists are paginated and report \
                 `truncated`; large content stays within a byte budget. Start with `status` to see local \
                 changes, `log` or `search_commits` for history, and `blame` or `file_history` for where code came from.",
            )
    }
}
