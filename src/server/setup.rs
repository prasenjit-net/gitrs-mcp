//! Repository setup: worktrees, submodules, configuration and object storage.

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};

use super::{
    GitServer,
    repos::{InRepo, RepoOnly},
};
use crate::tools;

#[tool_router(router = setup_router, vis = "pub(super)")]
impl GitServer {
    /// Every worktree of the repository, like `git worktree list`: the main one first, then linked
    /// worktrees with their path, HEAD commit, branch, and locked/prunable flags. `is_current` marks the
    /// one this server serves. Paginated (`limit`, default 100).
    #[tool(annotations(title = "List worktrees", read_only_hint = true))]
    async fn list_worktrees(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::worktrees::ListWorktreesArgs>>,
    ) -> Result<Json<tools::worktrees::ListWorktreesResult>, String> {
        self.repo(repo)?
            .run(move |r| tools::worktrees::list_worktrees(r, args))
            .await
            .map(Json)
    }

    /// Submodules declared in `.gitmodules`, like `git submodule status`: name, path, URL (passwords
    /// redacted), the commit the superproject records, the commit checked out, and whether the two
    /// match. Paginated (`limit`, default 100).
    #[tool(annotations(title = "List submodules", read_only_hint = true))]
    async fn list_submodules(
        &self,
        Parameters(InRepo { repo, args }): Parameters<
            InRepo<tools::submodules::ListSubmodulesArgs>,
        >,
    ) -> Result<Json<tools::submodules::ListSubmodulesResult>, String> {
        self.repo(repo)?
            .run(move |r| tools::submodules::list_submodules(r, args))
            .await
            .map(Json)
    }

    /// Git configuration values with the scope (system, global, local, worktree) and file each comes
    /// from: one `key` (e.g. `user.email`), a whole `section` (e.g. `remote.origin`), or everything.
    /// Secrets (credentials, tokens, passwords, auth headers, URL passwords) are redacted. Paginated (`limit`, default 100).
    #[tool(annotations(title = "Get config", read_only_hint = true))]
    async fn config_get(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::config::ConfigGetArgs>>,
    ) -> Result<Json<tools::config::ConfigGetResult>, String> {
        self.repo(repo)?
            .run(move |r| tools::config::config_get(r, args))
            .await
            .map(Json)
    }

    /// Object database statistics, like `git count-objects -v`: loose and packed object counts and
    /// sizes in bytes, the number of packs, and whether a commit-graph or multi-pack index exists.
    #[tool(annotations(title = "Count objects", read_only_hint = true))]
    async fn count_objects(
        &self,
        Parameters(RepoOnly { repo }): Parameters<RepoOnly>,
    ) -> Result<Json<tools::objects::CountObjectsResult>, String> {
        self.repo(repo)?
            .run(tools::objects::count_objects)
            .await
            .map(Json)
    }
}
