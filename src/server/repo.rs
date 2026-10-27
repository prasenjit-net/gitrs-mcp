//! Repository, refs and revisions.

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};

use super::GitServer;
use crate::tools;

#[tool_router(router = repo_router, vis = "pub(super)")]
impl GitServer {
    /// Repository overview: paths, HEAD, current branch, bareness, shallowness,
    /// and any in-progress operation (merge, rebase, cherry-pick, ...).
    #[tool(annotations(title = "Repository info", read_only_hint = true))]
    async fn repo_info(&self) -> Result<Json<tools::repo::RepoInfo>, String> {
        self.repo.run(tools::repo::repo_info).await.map(Json)
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
}
