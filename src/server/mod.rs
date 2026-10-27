//! MCP surface: tool registration and server metadata.
//!
//! Each tool is a thin async wrapper; the git logic lives in `crate::tools`
//! as plain sync functions over `&gix::Repository` so it can be tested directly.
//! Wrappers are grouped by area, one `#[tool_router]` per submodule, and combined here.

mod changes;
mod files;
mod history;
mod repo;
mod setup;

use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool_handler,
};

use crate::repo::RepoHandle;

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
            tool_router: Self::repo_router()
                + Self::changes_router()
                + Self::files_router()
                + Self::history_router()
                + Self::setup_router(),
        }
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
                 changes, `grep` to find code, `log` or `search_commits` for history, and `blame` or `file_history` for where code came from.",
            )
    }
}
