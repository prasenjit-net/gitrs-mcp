//! Tools that change the repository. This router is only registered with `--allow-write`.

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};

use super::GitServer;
use crate::tools;

#[tool_router(router = write_router, vis = "pub(super)")]
impl GitServer {
    /// Create a branch at a commit (default HEAD), like `git branch <name> [<start_point>]`. HEAD stays
    /// where it is; use `switch` to move to the branch. Refuses an existing name unless `force`.
    #[tool(annotations(
        title = "Create branch",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn create_branch(
        &self,
        Parameters(args): Parameters<tools::branches::CreateBranchArgs>,
    ) -> Result<Json<tools::branches::BranchResult>, String> {
        self.repo
            .run(move |r| tools::branches::create_branch(r, args))
            .await
            .map(Json)
    }

    /// Delete a local branch, like `git branch -d`. Refuses branches that are checked out, and branches
    /// whose commits aren't merged into their upstream (or HEAD) unless `force`. Dry run by default:
    /// set `dry_run: false` to delete. The result's `target` lets you recreate it.
    #[tool(annotations(
        title = "Delete branch",
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false
    ))]
    async fn delete_branch(
        &self,
        Parameters(args): Parameters<tools::branches::DeleteBranchArgs>,
    ) -> Result<Json<tools::branches::DeleteBranchResult>, String> {
        self.repo
            .run(move |r| tools::branches::delete_branch(r, args))
            .await
            .map(Json)
    }

    /// Rename a local branch (default: the current one), like `git branch -m`. Its reflog and
    /// `branch.<name>.*` config move with it, and HEAD follows if it's the current branch.
    #[tool(annotations(
        title = "Rename branch",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn rename_branch(
        &self,
        Parameters(args): Parameters<tools::branches::RenameBranchArgs>,
    ) -> Result<Json<tools::branches::RenameBranchResult>, String> {
        self.repo
            .run(move |r| tools::branches::rename_branch(r, args))
            .await
            .map(Json)
    }

    /// Create a tag at a revision (default HEAD): annotated when `message` is given (records you as
    /// tagger), lightweight otherwise. Refuses an existing tag unless `force`. Returns the tag as
    /// `show_tag` would.
    #[tool(annotations(
        title = "Create tag",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn create_tag(
        &self,
        Parameters(args): Parameters<tools::branches::CreateTagArgs>,
    ) -> Result<Json<tools::refs::ShowTagResult>, String> {
        self.repo
            .run(move |r| tools::branches::create_tag(r, args))
            .await
            .map(Json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{repo::RepoHandle, server::Policy, tools::testutil::fixture_repo};

    #[tokio::test]
    async fn write_wrappers_return_results_and_tool_errors() {
        let (dir, _) = fixture_repo();
        let server = GitServer::new(
            RepoHandle::open_isolated(dir.path()),
            Policy { allow_write: true },
        );
        let created = server
            .create_branch(Parameters(tools::branches::CreateBranchArgs {
                name: "topic".into(),
                start_point: None,
                force: false,
            }))
            .await
            .unwrap();
        assert_eq!(created.0.short_name, "topic");

        let err = server
            .delete_branch(Parameters(tools::branches::DeleteBranchArgs {
                name: "nope".into(),
                force: false,
                dry_run: true,
            }))
            .await
            .err()
            .expect("the call is refused");
        assert!(err.contains("no branch `nope`"), "{err}");
        let err = server
            .rename_branch(Parameters(tools::branches::RenameBranchArgs {
                old: Some("topic".into()),
                new: "main".into(),
                force: false,
            }))
            .await
            .err()
            .expect("the call is refused");
        assert!(err.contains("already exists"), "{err}");
        let err = server
            .create_tag(Parameters(tools::branches::CreateTagArgs {
                name: "v1".into(),
                target: None,
                message: None,
                force: false,
            }))
            .await
            .err()
            .expect("the call is refused");
        assert!(err.contains("already exists"), "{err}");
    }
}
