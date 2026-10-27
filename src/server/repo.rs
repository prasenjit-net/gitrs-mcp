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

    /// Name a commit after the nearest tag in its history, like `git describe --always`: `v1.2` on a
    /// tagged commit, `v1.2-3-gabc1234` three commits later, or the short SHA when no tag is reachable.
    /// Uses annotated tags unless `tags: true`. Use it to see which release a commit belongs to.
    #[tool(annotations(title = "Describe", read_only_hint = true))]
    async fn describe(
        &self,
        Parameters(args): Parameters<tools::refs::DescribeArgs>,
    ) -> Result<Json<tools::refs::DescribeResult>, String> {
        self.repo
            .run(move |r| tools::refs::describe(r, args))
            .await
            .map(Json)
    }

    /// Details of one tag: annotated or lightweight, the object it points at (and the commit it peels
    /// to), and for annotated tags the tagger, date, message and whether it is signed.
    #[tool(annotations(title = "Show tag", read_only_hint = true))]
    async fn show_tag(
        &self,
        Parameters(args): Parameters<tools::refs::ShowTagArgs>,
    ) -> Result<Json<tools::refs::ShowTagResult>, String> {
        self.repo
            .run(move |r| tools::refs::show_tag(r, args))
            .await
            .map(Json)
    }

    /// Where a ref (default HEAD) pointed over time, newest first, like `git reflog`: each update's old
    /// and new commit, who made it and git's message (commit, checkout, reset, rebase, ...). Use it to
    /// find lost commits or see recent branch switches. Paginated (`limit`, default 20).
    #[tool(annotations(title = "Reflog", read_only_hint = true))]
    async fn reflog(
        &self,
        Parameters(args): Parameters<tools::reflog::ReflogArgs>,
    ) -> Result<Json<tools::reflog::ReflogResult>, String> {
        self.repo
            .run(move |r| tools::reflog::reflog(r, args))
            .await
            .map(Json)
    }

    /// The stash list, newest first, like `git stash list`: each stash's `stash@{n}` selector, commit,
    /// base commit, branch and message. Inspect one with `show_commit` or `diff`. Paginated (`limit`, default 20).
    #[tool(annotations(title = "List stashes", read_only_hint = true))]
    async fn list_stashes(
        &self,
        Parameters(args): Parameters<tools::reflog::ListStashesArgs>,
    ) -> Result<Json<tools::reflog::ListStashesResult>, String> {
        self.repo
            .run(move |r| tools::reflog::list_stashes(r, args))
            .await
            .map(Json)
    }
}
