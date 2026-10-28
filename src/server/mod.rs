//! MCP surface: tool registration and server metadata.
//!
//! Each tool is a thin async wrapper; the git logic lives in `crate::tools`
//! as plain sync functions over `&gix::Repository` so it can be tested directly.
//! Wrappers are grouped by area, one `#[tool_router]` per submodule, and combined here.

mod changes;
mod files;
mod history;
mod progress;
mod prompts;
mod repo;
mod resources;
mod setup;
mod write;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::router::{prompt::PromptRouter, tool::ToolRouter},
    model::{
        CompleteRequestParams, CompleteResult, Implementation, ListResourceTemplatesResult,
        ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ServerCapabilities, ServerConfig,
    },
    prompt_handler,
    service::RequestContext,
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
    prompt_router: PromptRouter<Self>,
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
            prompt_router: Self::prompts_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .enable_completions()
            .build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.instructions())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        self.resource_list().await
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(self.resource_templates())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.resource_read(request.uri).await.map(Into::into)
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        Ok(self.completion(request).await)
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

    const WRITE_TOOLS: [&str; 22] = [
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
        "merge",
        "cherry_pick",
        "revert",
        "rebase",
        "continue_operation",
        "abort_operation",
        "worktree_add",
        "worktree_remove",
    ];

    /// A client connected to a server on `path` over an in-memory pipe, speaking the real protocol.
    pub(crate) async fn connect(
        path: &std::path::Path,
        allow_write: bool,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        connect_with(path, allow_write, ()).await
    }

    /// Like [`connect`], with `handler` receiving the server's notifications.
    pub(crate) async fn connect_with<C: rmcp::ClientHandler>(
        path: &std::path::Path,
        allow_write: bool,
        handler: C,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, C> {
        use rmcp::ServiceExt;
        let (server_side, client_side) = tokio::io::duplex(1 << 16);
        let server = server(path, allow_write);
        tokio::spawn(async move {
            let running = server.serve(server_side).await?;
            running.waiting().await?;
            anyhow::Ok(())
        });
        handler.serve(client_side).await.expect("client connects")
    }

    /// A client that records the progress notifications it receives.
    #[derive(Clone, Default)]
    struct ProgressRecorder(
        std::sync::Arc<std::sync::Mutex<Vec<rmcp::model::ProgressNotificationParam>>>,
    );

    impl rmcp::ClientHandler for ProgressRecorder {
        async fn on_progress(
            &self,
            params: rmcp::model::ProgressNotificationParam,
            _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
        ) {
            self.0.lock().unwrap().push(params);
        }
    }

    #[tokio::test]
    async fn long_tools_send_progress_for_a_progress_token() {
        use rmcp::model::{CallToolRequestParams, ClientRequest, Request};
        let (dir, _) = fixture_repo();
        let recorder = ProgressRecorder::default();
        let client = connect_with(dir.path(), false, recorder.clone()).await;
        let mut arguments = serde_json::Map::new();
        arguments.insert("path".into(), "a.txt".into());
        let call = ClientRequest::CallToolRequest(Request::new(
            CallToolRequestParams::new("blame").with_arguments(arguments),
        ));
        let handle = client
            .send_cancellable_request(call, rmcp::service::PeerRequestOptions::no_options())
            .await
            .unwrap();
        let token = handle.progress_token.clone();
        handle.await_response().await.unwrap();
        // Notifications are sent before the response, but handled on their own task.
        for _ in 0..100 {
            if !recorder.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let seen = recorder.0.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].progress_token, token);
        assert_eq!(seen[0].message.as_deref(), Some("blaming a.txt (2 lines)"));
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn resources_and_completion_over_the_protocol() {
        use rmcp::model::{
            ArgumentInfo, CompleteRequestParams, ErrorCode, ReadResourceRequestParams, Reference,
        };
        let (dir, _) = fixture_repo();
        let client = connect(dir.path(), false).await;
        let listed = client.list_all_resources().await.unwrap();
        assert_eq!(listed.len(), 3);
        let templates = client.list_all_resource_templates().await.unwrap();
        assert_eq!(templates.len(), 3);

        let read = client
            .read_resource(ReadResourceRequestParams::new("gitrs://diff/HEAD~1..HEAD"))
            .await
            .unwrap();
        let text = serde_json::to_value(&read.contents).unwrap()[0]["text"].clone();
        assert!(
            text.as_str()
                .unwrap()
                .starts_with("diff --git a/a.txt b/a.txt\n")
        );

        let err = client
            .read_resource(ReadResourceRequestParams::new("gitrs://nope"))
            .await
            .unwrap_err();
        match err {
            rmcp::ServiceError::McpError(e) => assert_eq!(e.code, ErrorCode::RESOURCE_NOT_FOUND),
            other => panic!("expected an MCP error, got {other:?}"),
        }

        let found = client
            .complete(CompleteRequestParams::new(
                Reference::for_resource("gitrs://commit/{rev}"),
                ArgumentInfo::new("rev", "v"),
            ))
            .await
            .unwrap();
        assert_eq!(found.completion.values, ["v1"]);
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn prompts_over_the_protocol() {
        use rmcp::model::{ErrorCode, GetPromptRequestParams};
        let (dir, _) = fixture_repo();
        let client = connect(dir.path(), false).await;
        let prompts = client.list_all_prompts().await.unwrap();
        let names: Vec<_> = prompts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["explain_history", "review_changes", "write_commit_message"]
        );
        let history = &prompts[0];
        let required: Vec<_> = history
            .arguments
            .iter()
            .flatten()
            .map(|a| (a.name.as_str(), a.required))
            .collect();
        assert!(required.contains(&("path", Some(true))));
        assert!(required.contains(&("limit", Some(false))));

        let mut arguments = serde_json::Map::new();
        arguments.insert("path".into(), "a.txt".into());
        let got = client
            .get_prompt(GetPromptRequestParams::new("explain_history").with_arguments(arguments))
            .await
            .unwrap();
        let text = serde_json::to_value(&got.messages).unwrap()[0]["content"]["text"].clone();
        assert!(text.as_str().unwrap().contains("\"second commit\""));

        let mut sides = serde_json::Map::new();
        sides.insert("from".into(), "HEAD~1".into());
        sides.insert("to".into(), "HEAD".into());
        let review = client
            .get_prompt(GetPromptRequestParams::new("review_changes").with_arguments(sides))
            .await
            .unwrap();
        let text = serde_json::to_value(&review.messages).unwrap()[0]["content"]["text"].clone();
        assert!(text.as_str().unwrap().contains("+two"));

        let refused = client
            .get_prompt(GetPromptRequestParams::new("write_commit_message"))
            .await
            .unwrap_err();
        match refused {
            rmcp::ServiceError::McpError(e) => {
                assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
                assert!(e.message.contains("nothing is staged"));
            }
            other => panic!("expected an MCP error, got {other:?}"),
        }
        client.cancel().await.unwrap();
    }

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
                    "delete_branch"
                        | "restore"
                        | "reset"
                        | "clean"
                        | "stash_drop"
                        | "worktree_remove"
                )),
                "{name}"
            );
        }
        assert!(writable.instructions().contains("Write tools are enabled"));
    }
}
