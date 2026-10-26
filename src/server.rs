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
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Local-only Git tools for a single repository. All output is structured JSON. \
                 Revisions accept anything `git rev-parse` would (SHA prefixes, branches, tags, HEAD~n).",
            )
    }
}
