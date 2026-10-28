//! Prompts: ready-made requests filled in with repository data. The text is built in `tools::prompts`.

use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{GetPromptResult, PromptMessage, Role},
    prompt, prompt_router,
};

use super::{
    GitServer,
    repos::{InRepo, RepoOnly},
};
use crate::tools;

/// A prompt result with one user message. Failures (nothing staged, a bad revision) are invalid
/// parameters, with a message saying what to change.
fn user_prompt(
    description: &str,
    text: Result<String, String>,
) -> Result<GetPromptResult, McpError> {
    let text = text.map_err(|e| McpError::invalid_params(e, None))?;
    Ok(
        GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)])
            .with_description(description),
    )
}

#[prompt_router(router = "prompts_router", vis = "pub(super)")]
impl GitServer {
    /// Write a commit message for the staged changes. Fills in the staged diff and the repository's
    /// recent commit subjects as a style guide. Fails when nothing is staged.
    #[prompt(title = "Write commit message")]
    async fn write_commit_message(
        &self,
        Parameters(RepoOnly { repo }): Parameters<RepoOnly>,
    ) -> Result<GetPromptResult, McpError> {
        let text = match self.repo(repo) {
            Ok(handle) => handle.run(tools::prompts::write_commit_message).await,
            Err(err) => Err(err),
        };
        user_prompt("Commit message for the staged changes", text)
    }

    /// Review a set of changes: the diff (default the staged changes, `HEAD..index`) plus the blame of
    /// the lines each hunk replaces, so the reviewer sees which commits the change touches.
    #[prompt(title = "Review changes")]
    async fn review_changes(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::prompts::ReviewArgs>>,
    ) -> Result<GetPromptResult, McpError> {
        let text = match self.repo(repo) {
            Ok(handle) => {
                handle
                    .run(move |r| tools::prompts::review_changes(r, args))
                    .await
            }
            Err(err) => Err(err),
        };
        user_prompt("Code review of a diff, with blame context", text)
    }

    /// Explain how a file evolved and why: its history (following renames) with commit messages, and
    /// the patches of its most recent commits.
    #[prompt(title = "Explain history")]
    async fn explain_history(
        &self,
        Parameters(InRepo { repo, args }): Parameters<InRepo<tools::prompts::HistoryArgs>>,
    ) -> Result<GetPromptResult, McpError> {
        let text = match self.repo(repo) {
            Ok(handle) => {
                handle
                    .run(move |r| tools::prompts::explain_history(r, args))
                    .await
            }
            Err(err) => Err(err),
        };
        user_prompt("How a file evolved, from its history", text)
    }
}
