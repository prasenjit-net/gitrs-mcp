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
mod write;

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
    pub allow_write: bool,
}

#[derive(Clone)]
pub struct GitServer {
    repo: RepoHandle,
    policy: Policy,
    tool_router: ToolRouter<Self>,
}

impl GitServer {
    pub fn new(repo: RepoHandle, policy: Policy) -> Self {
        let mut tool_router = Self::repo_router()
            + Self::changes_router()
            + Self::files_router()
            + Self::history_router()
            + Self::setup_router();
        // Without `--allow-write` the write tools aren't registered at all, so a client never sees them.
        if policy.allow_write {
            tool_router += Self::write_router();
        }
        Self {
            repo,
            policy,
            tool_router,
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.instructions())
    }
}

impl GitServer {
    fn instructions(&self) -> String {
        let mut text = String::from(
            "Local-only Git tools for a single repository. All output is structured JSON. \
             Revisions accept anything `git rev-parse` would (SHA prefixes, branches, tags, HEAD~n). \
             Paths and pathspecs are relative to the repository root. Lists are paginated and report \
             `truncated`; large content stays within a byte budget. Start with `status` to see local \
             changes, `grep` to find code, `log` or `search_commits` for history, `blame` or `file_history` \
             for where code came from, and `merge_preview` before merging.",
        );
        if self.policy.allow_write {
            text.push_str(
                " Write tools are enabled. They refuse instead of overwriting work, destructive ones \
                 default to `dry_run: true`, and each returns the new state.",
            );
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::fixture_repo;

    const WRITE_TOOLS: [&str; 14] = [
        "create_branch",
        "delete_branch",
        "rename_branch",
        "create_tag",
        "stage",
        "unstage",
        "commit",
        "reset",
        "restore",
        "switch",
        "clean",
        "stash_push",
        "stash_pop",
        "stash_drop",
    ];

    fn server(path: &std::path::Path, allow_write: bool) -> GitServer {
        GitServer::new(RepoHandle::open_isolated(path), Policy { allow_write })
    }

    fn read_only(tool: &rmcp::model::Tool) -> Option<bool> {
        tool.annotations.as_ref().and_then(|a| a.read_only_hint)
    }

    #[test]
    fn write_tools_are_registered_only_with_allow_write() {
        let (dir, _) = fixture_repo();
        let plain = server(dir.path(), false);
        let tools = plain.tool_router.list_all();
        assert!(
            tools
                .iter()
                .all(|t| !WRITE_TOOLS.contains(&t.name.as_ref()))
        );
        assert!(
            tools.iter().all(|t| read_only(t) == Some(true)),
            "every default tool is read-only"
        );
        assert!(!plain.instructions().contains("Write tools"));

        let writable = server(dir.path(), true);
        assert_eq!(
            writable.tool_router.list_all().len(),
            tools.len() + WRITE_TOOLS.len()
        );
        for name in WRITE_TOOLS {
            let tool = writable.tool_router.get(name).unwrap();
            assert_eq!(read_only(tool), Some(false), "{name}");
            let destructive = tool.annotations.as_ref().and_then(|a| a.destructive_hint);
            assert_eq!(
                destructive,
                Some(matches!(
                    name,
                    "delete_branch" | "restore" | "reset" | "clean" | "stash_drop"
                )),
                "{name}"
            );
        }
        assert!(writable.instructions().contains("Write tools are enabled"));
    }
}
