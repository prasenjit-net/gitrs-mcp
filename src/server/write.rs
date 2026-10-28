//! Tools that change the repository. This router is only registered with `--allow-write`.

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_router,
};

use super::{
    GitServer,
    repos::{InRepo, RepoOnly},
};
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
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::branches::CreateBranchArgs>>,
    ) -> Result<Json<tools::branches::BranchResult>, String> {
        self.writable(repo)?
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
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::branches::DeleteBranchArgs>>,
    ) -> Result<Json<tools::branches::DeleteBranchResult>, String> {
        self.writable(repo)?
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
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::branches::RenameBranchArgs>>,
    ) -> Result<Json<tools::branches::RenameBranchResult>, String> {
        self.writable(repo)?
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
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::branches::CreateTagArgs>>,
    ) -> Result<Json<tools::refs::ShowTagResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::branches::create_tag(r, args))
            .await
            .map(Json)
    }

    /// Stage working-tree changes for the next commit, like `git add <paths>` (or `git add -A` with
    /// `all: true`): modifications, deletions and new files, with `.gitattributes` filters applied.
    /// Staging a conflicted file marks it resolved. Ignored files named explicitly are skipped and
    /// reported. Returns what changed and how many paths are now staged.
    #[tool(annotations(
        title = "Stage",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn stage(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::staging::StageArgs>>,
    ) -> Result<Json<tools::staging::IndexUpdateResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::staging::stage(r, args))
            .await
            .map(Json)
    }

    /// Undo staged changes, resetting the index entries for `paths` (or `all: true`) to HEAD, like
    /// `git restore --staged`. The working tree is not touched, so the changes stay as unstaged edits.
    #[tool(annotations(
        title = "Unstage",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true
    ))]
    async fn unstage(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::staging::UnstageArgs>>,
    ) -> Result<Json<tools::staging::IndexUpdateResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::staging::unstage(r, args))
            .await
            .map(Json)
    }

    /// Record the staged changes as a new commit on the current branch, like `git commit -m`
    /// (`amend: true` replaces the last commit instead). Uses the configured identity; `author` can
    /// credit someone else. Refuses an empty commit unless `allow_empty`, unresolved conflicts, and
    /// repositories that require signed commits. Hooks aren't run; existing ones are reported.
    #[tool(annotations(
        title = "Commit",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn commit(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::commit::CommitArgs>>,
    ) -> Result<Json<tools::commit::CommitResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::commit::commit(r, args))
            .await
            .map(Json)
    }

    /// Move the current branch (or detached HEAD) to another commit, like `git reset`: `mixed` (the
    /// default) also resets the index so the undone changes become unstaged; `soft` keeps them staged;
    /// `hard` also overwrites the working tree, so it's a dry run unless `dry_run: false` and reports
    /// which uncommitted changes would be lost. The result's `previous` undoes the move.
    #[tool(annotations(
        title = "Reset",
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false
    ))]
    async fn reset(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::commit::ResetArgs>>,
    ) -> Result<Json<tools::commit::ResetResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::commit::reset(r, args))
            .await
            .map(Json)
    }

    /// Restore files from the index (default) or a revision, like `git restore`: working-tree files by
    /// default, the index with `staged: true`, or both. Overwrites local edits, so it's a dry run by
    /// default: check `files` and `discards_changes`, then set `dry_run: false`.
    #[tool(annotations(
        title = "Restore",
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true
    ))]
    async fn restore(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::checkout::RestoreArgs>>,
    ) -> Result<Json<tools::checkout::RestoreResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::checkout::restore(r, args))
            .await
            .map(Json)
    }

    /// Switch HEAD to another branch (or `create` one, or `detach` at a commit) and update the working
    /// tree and index, like `git switch`. Local changes to files the switch doesn't touch are carried
    /// over; if it would overwrite any local change it refuses and names the files.
    #[tool(annotations(
        title = "Switch",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn switch(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::checkout::SwitchArgs>>,
    ) -> Result<Json<tools::checkout::SwitchResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::checkout::switch(r, args))
            .await
            .map(Json)
    }

    /// Delete untracked files from the working tree, like `git clean -f`: `directories: true` adds
    /// untracked directories (`-d`), `ignored` adds ignored files (`also`, like `-x`) or only those
    /// (`only`, like `-X`). Nested repositories are never removed. Dry run by default: check
    /// `removed`, then set `dry_run: false`.
    #[tool(annotations(
        title = "Clean",
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true
    ))]
    async fn clean(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::clean::CleanArgs>>,
    ) -> Result<Json<tools::clean::CleanResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::clean::clean(r, args))
            .await
            .map(Json)
    }

    /// Save local changes (staged and unstaged, plus untracked files with `include_untracked`) as a
    /// new stash and reset to HEAD, like `git stash push`. Stashes use git's layout, so git can pop them.
    #[tool(annotations(
        title = "Stash push",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn stash_push(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::stash::StashPushArgs>>,
    ) -> Result<Json<tools::reflog::StashEntry>, String> {
        self.writable(repo)?
            .run(move |r| tools::stash::stash_push(r, args))
            .await
            .map(Json)
    }

    /// Apply a stash (default the newest) to the working tree and drop it, like `git stash pop`;
    /// `drop: false` keeps it (`git stash apply`), `restore_index: true` restores staged changes as
    /// staged. Refuses instead of conflicting or overwriting local changes, naming the files.
    #[tool(annotations(
        title = "Stash pop",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn stash_pop(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::stash::StashPopArgs>>,
    ) -> Result<Json<tools::stash::StashPopResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::stash::stash_pop(r, args))
            .await
            .map(Json)
    }

    /// Remove a stash (default the newest) from the stash list, like `git stash drop`. Dry run by
    /// default: check `stash`, then set `dry_run: false`. Its `id` can bring it back with `git stash store`.
    #[tool(annotations(
        title = "Stash drop",
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false
    ))]
    async fn stash_drop(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::stash::StashDropArgs>>,
    ) -> Result<Json<tools::stash::StashDropResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::stash::stash_drop(r, args))
            .await
            .map(Json)
    }

    /// Merge a branch or commit into the current branch, like `git merge`: fast-forwards when possible
    /// (`fast_forward: never` always makes a merge commit, `only` refuses otherwise). On conflicts it
    /// stops with git's merge state; resolve, `stage`, then `continue_operation` (or `abort_operation`).
    /// Refuses up front if the index has staged changes or the merge would overwrite local changes.
    #[tool(annotations(
        title = "Merge",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn merge(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::integrate::MergeArgs>>,
    ) -> Result<Json<tools::integrate::OperationResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::integrate::merge(r, args))
            .await
            .map(Json)
    }

    /// Apply the change one commit made onto the current branch as a new commit, like `git cherry-pick`.
    /// Stops on conflicts with git's cherry-pick state; resolve, `stage`, then `continue_operation`.
    #[tool(annotations(
        title = "Cherry-pick",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn cherry_pick(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::integrate::PickArgs>>,
    ) -> Result<Json<tools::integrate::OperationResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::integrate::cherry_pick(r, args))
            .await
            .map(Json)
    }

    /// Commit the inverse of one commit's change, like `git revert`. Stops on conflicts with git's revert
    /// state; resolve, `stage`, then `continue_operation`.
    #[tool(annotations(
        title = "Revert",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn revert(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::integrate::PickArgs>>,
    ) -> Result<Json<tools::integrate::OperationResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::integrate::revert(r, args))
            .await
            .map(Json)
    }

    /// Replay the current branch's commits onto another commit, like `git rebase <onto>`; `todo` gives an
    /// explicit plan (pick, reword, squash, fixup, drop) like `git rebase -i`. Stops on conflicts with
    /// git's rebase state, so `continue_operation` or `git rebase --continue` can finish it.
    #[tool(annotations(
        title = "Rebase",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn rebase(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::rebase::RebaseArgs>>,
    ) -> Result<Json<tools::integrate::OperationResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::rebase::rebase(r, args))
            .await
            .map(Json)
    }

    /// Conclude the merge, cherry-pick, revert or rebase that stopped on conflicts, once every conflict is
    /// resolved and staged, like `git commit` / `git rebase --continue` (`skip: true` for
    /// `git rebase --skip`). Works on operations git started too.
    #[tool(annotations(
        title = "Continue",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn continue_operation(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::integrate::ContinueArgs>>,
    ) -> Result<Json<tools::integrate::OperationResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::integrate::continue_operation(r, args))
            .await
            .map(Json)
    }

    /// Abandon the merge, cherry-pick, revert or rebase in progress and return to where it started, like
    /// `git merge --abort` / `git rebase --abort`. Conflict resolutions in progress are discarded; local
    /// changes to files the operation didn't touch are kept.
    #[tool(annotations(
        title = "Abort",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn abort_operation(
        &self,
        Parameters(RepoOnly { repo }): Parameters<RepoOnly>,
    ) -> Result<Json<tools::integrate::OperationResult>, String> {
        self.writable(repo)?
            .run(tools::integrate::abort_operation)
            .await
            .map(Json)
    }

    /// Create a linked worktree (a second checkout of this repository) at `path`, on an existing branch,
    /// a new branch (`create`), or a detached commit (`detach`), like `git worktree add`.
    #[tool(annotations(
        title = "Add worktree",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false
    ))]
    async fn worktree_add(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::worktrees::WorktreeAddArgs>>,
    ) -> Result<Json<tools::worktrees::WorktreeEntry>, String> {
        self.writable(repo)?
            .run(move |r| tools::worktrees::worktree_add(r, args))
            .await
            .map(Json)
    }

    /// Delete a linked worktree's checkout and registration, like `git worktree remove`; its branch is
    /// kept. Refuses dirty or locked worktrees unless `force`. Dry run by default.
    #[tool(annotations(
        title = "Remove worktree",
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false
    ))]
    async fn worktree_remove(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::worktrees::WorktreeRemoveArgs>>,
    ) -> Result<Json<tools::worktrees::WorktreeRemoveResult>, String> {
        self.writable(repo)?
            .run(move |r| tools::worktrees::worktree_remove(r, args))
            .await
            .map(Json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{repo::RepoHandle, server::Policy, tools::testutil::fixture_repo};

    #[tokio::test]
    async fn write_tools_refuse_untrusted_repositories() {
        use crate::tools::testutil::{assert_unchanged, snapshot, write};
        let (dir, _) = fixture_repo();
        write(dir.path(), "new.txt", "unstaged\n");
        let server = GitServer::serving(
            vec![RepoHandle::open_with_trust(
                dir.path(),
                gix::sec::Trust::Reduced,
            )],
            Policy {
                allow_write: true,
                watch_interval: None,
            },
        );
        let before = snapshot(dir.path());
        let staged = server
            .stage(in_default(tools::staging::StageArgs {
                paths: vec!["new.txt".into()],
                all: false,
            }))
            .await;
        let err = staged.err().expect("staging is refused");
        assert!(err.contains("safe.directory"), "{err}");
        let aborted = server
            .abort_operation(Parameters(RepoOnly::default()))
            .await;
        assert!(aborted.err().unwrap().contains("safe.directory"));
        assert_unchanged(dir.path(), &before);
        // Reads still work.
        let info = server
            .repo(None)
            .unwrap()
            .run(tools::repo::repo_info)
            .await
            .unwrap();
        assert!(!info.trusted);
    }

    /// Every write tool picks its repository with the trust check.
    #[test]
    fn every_write_wrapper_checks_trust() {
        let source = include_str!("write.rs");
        let wrappers = &source[..source.find("#[cfg(test)]").unwrap()];
        assert!(
            !wrappers.contains("self.repo("),
            "use self.writable(repo) in write wrappers"
        );
        assert_eq!(wrappers.matches("self.writable(repo)?").count(), 22);
    }

    /// Tool arguments for the default repository.
    fn in_default<T>(args: T) -> Parameters<InRepo<T>> {
        Parameters(InRepo { repo: None, args })
    }

    #[tokio::test]
    async fn write_wrappers_return_results_and_tool_errors() {
        let (dir, _) = fixture_repo();
        let server = GitServer::new(
            RepoHandle::open_isolated(dir.path()),
            Policy {
                allow_write: true,
                watch_interval: None,
            },
        );
        let created = server
            .create_branch(in_default(tools::branches::CreateBranchArgs {
                name: "topic".into(),
                start_point: None,
                force: false,
            }))
            .await
            .unwrap();
        assert_eq!(created.0.short_name, "topic");

        let err = server
            .delete_branch(in_default(tools::branches::DeleteBranchArgs {
                name: "nope".into(),
                force: false,
                dry_run: true,
            }))
            .await
            .err()
            .expect("the call is refused");
        assert!(err.contains("no branch `nope`"), "{err}");
        let err = server
            .rename_branch(in_default(tools::branches::RenameBranchArgs {
                old: Some("topic".into()),
                new: "main".into(),
                force: false,
            }))
            .await
            .err()
            .expect("the call is refused");
        assert!(err.contains("already exists"), "{err}");
        let err = server
            .create_tag(in_default(tools::branches::CreateTagArgs {
                name: "v1".into(),
                target: None,
                message: None,
                force: false,
            }))
            .await
            .err()
            .expect("the call is refused");
        assert!(err.contains("already exists"), "{err}");

        crate::tools::testutil::write(dir.path(), "a.txt", "edited\n");
        let staged = server
            .stage(in_default(tools::staging::StageArgs {
                paths: vec!["a.txt".into()],
                all: false,
            }))
            .await
            .unwrap();
        assert_eq!(staged.0.staged_total, 1);
        let unstaged = server
            .unstage(in_default(tools::staging::UnstageArgs {
                paths: vec![],
                all: true,
            }))
            .await
            .unwrap();
        assert_eq!(unstaged.0.staged_total, 0);

        server
            .stage(in_default(tools::staging::StageArgs {
                paths: vec![],
                all: true,
            }))
            .await
            .unwrap();
        let committed = server
            .commit(in_default(tools::commit::CommitArgs {
                message: "Through MCP".into(),
                amend: false,
                allow_empty: false,
                author: None,
            }))
            .await
            .unwrap();
        assert_eq!(committed.0.commit.summary, "Through MCP");
        let reset = server
            .reset(in_default(tools::commit::ResetArgs {
                rev: Some("HEAD~1".into()),
                mode: tools::commit::ResetMode::Soft,
                dry_run: None,
            }))
            .await
            .unwrap();
        assert_eq!(reset.0.previous, committed.0.commit.id);

        let restored = server
            .restore(in_default(tools::checkout::RestoreArgs {
                paths: vec![".".into()],
                source: None,
                staged: false,
                worktree: None,
                dry_run: true,
            }))
            .await
            .unwrap();
        assert!(restored.0.dry_run);
        let switched = server
            .switch(in_default(tools::checkout::SwitchArgs {
                target: "feature".into(),
                create: false,
                start_point: None,
                detach: false,
            }))
            .await
            .unwrap();
        assert_eq!(switched.0.branch.as_deref(), Some("feature"));

        crate::tools::testutil::write(dir.path(), "a.txt", "to stash\n");
        let stashed = server
            .stash_push(in_default(tools::stash::StashPushArgs {
                message: None,
                include_untracked: false,
            }))
            .await
            .unwrap();
        assert_eq!(stashed.0.selector, "stash@{0}");
        let dropped = server
            .stash_drop(in_default(tools::stash::StashDropArgs {
                index: 0,
                dry_run: true,
            }))
            .await
            .unwrap();
        assert!(dropped.0.dry_run);
        let popped = server
            .stash_pop(in_default(tools::stash::StashPopArgs {
                index: 0,
                restore_index: false,
                drop: true,
            }))
            .await
            .unwrap();
        assert!(popped.0.dropped);

        let merged = server
            .merge(in_default(tools::integrate::MergeArgs {
                theirs: "main".into(),
                message: None,
                fast_forward: tools::integrate::FastForward::Allow,
            }))
            .await
            .unwrap();
        assert_eq!(merged.0.status, tools::integrate::OperationStatus::UpToDate);
        let pick = tools::integrate::PickArgs { rev: "nope".into() };
        assert!(server.cherry_pick(in_default(pick)).await.is_err());
        let pick = tools::integrate::PickArgs { rev: "nope".into() };
        assert!(server.revert(in_default(pick)).await.is_err());
        crate::tools::testutil::run_git(dir.path(), &["checkout", "--", "a.txt"]);
        let rebased = server
            .rebase(in_default(tools::rebase::RebaseArgs {
                onto: "main".into(),
                upstream: None,
                todo: None,
            }))
            .await
            .unwrap();
        assert_eq!(
            rebased.0.status,
            tools::integrate::OperationStatus::UpToDate
        );
        let args = tools::integrate::ContinueArgs {
            message: None,
            skip: false,
        };
        assert!(server.continue_operation(in_default(args)).await.is_err());
        assert!(
            server
                .abort_operation(Parameters(RepoOnly::default()))
                .await
                .is_err()
        );
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("wt");
        let added = server
            .worktree_add(in_default(tools::worktrees::WorktreeAddArgs {
                path: path.display().to_string(),
                target: "spare".into(),
                create: true,
                start_point: None,
                detach: false,
            }))
            .await
            .unwrap();
        assert_eq!(added.0.branch.as_deref(), Some("spare"));
        let removed = server
            .worktree_remove(in_default(tools::worktrees::WorktreeRemoveArgs {
                worktree: path.display().to_string(),
                force: false,
                dry_run: true,
            }))
            .await
            .unwrap();
        assert!(removed.0.dry_run);
    }
}
