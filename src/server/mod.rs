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
mod watch;
mod write;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::router::{prompt::PromptRouter, tool::ToolRouter},
    model::{
        CompleteRequestParams, CompleteResult, Implementation, ListResourceTemplatesResult,
        ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ServerCapabilities, ServerConfig, SubscribeRequestParams,
        SubscriptionFilter, UnsubscribeRequestParams,
    },
    prompt_handler,
    service::{RequestContext, SubscriptionContext},
    tool_handler,
};

use crate::repo::RepoHandle;

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Gate for every tool that mutates the repo, index, or worktree.
    pub allow_write: bool,
    /// With `--watch`, how often subscribed resources are checked for changes; `None` turns
    /// resource subscriptions off.
    pub watch_interval: Option<std::time::Duration>,
}

#[derive(Clone)]
pub struct GitServer {
    repo: RepoHandle,
    policy: Policy,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
    watcher: Option<std::sync::Arc<watch::Watcher>>,
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
        let watcher = policy
            .watch_interval
            .map(|interval| watch::Watcher::new(repo.clone(), interval));
        Self {
            repo,
            policy,
            tool_router,
            prompt_router: Self::prompts_router(),
            watcher,
        }
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        let builder = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources();
        let builder = if self.watcher.is_some() {
            builder.enable_resources_subscribe()
        } else {
            builder
        };
        let capabilities = builder.enable_completions().build();
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

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.subscribe_legacy(request.uri, context.peer).await
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.unsubscribe_legacy(&request.uri)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        self.listen_filter(requested)
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        self.listen_stream(context).await
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

    /// A client that records the resource updates it receives.
    #[derive(Clone, Default)]
    struct UpdateRecorder(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl rmcp::ClientHandler for UpdateRecorder {
        async fn on_resource_updated(
            &self,
            params: rmcp::model::ResourceUpdatedNotificationParam,
            _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
        ) {
            self.0.lock().unwrap().push(params.uri);
        }
    }

    impl UpdateRecorder {
        /// Wait up to two seconds for `count` updates in all.
        async fn wait_for(&self, count: usize) -> Vec<String> {
            for _ in 0..200 {
                if self.0.lock().unwrap().len() >= count {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            self.0.lock().unwrap().clone()
        }
    }

    /// A server on `path` that checks subscriptions every 20 ms. With `modern`, the client uses
    /// protocol 2026-07-28 (no `initialize`), which `subscriptions/listen` needs.
    async fn watching<C: rmcp::ClientHandler>(
        path: &std::path::Path,
        handler: C,
        modern: bool,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, C> {
        use rmcp::ServiceExt;
        let (server_side, client_side) = tokio::io::duplex(1 << 16);
        let server = GitServer::new(
            RepoHandle::open_isolated(path),
            Policy {
                allow_write: false,
                watch_interval: Some(std::time::Duration::from_millis(20)),
            },
        );
        tokio::spawn(async move {
            let running = server.serve(server_side).await?;
            running.waiting().await?;
            anyhow::Ok(())
        });
        if !modern {
            return handler.serve(client_side).await.expect("client connects");
        }
        use rmcp::service::ClientServiceExt;
        let lifecycle = rmcp::ClientLifecycleMode::Discover {
            preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
        };
        handler
            .serve_with_lifecycle(client_side, lifecycle)
            .await
            .expect("client connects")
    }

    // The legacy subscribe requests are what today's clients send; rmcp marks them deprecated in
    // favour of `subscriptions/listen`, which the next test covers.
    #[allow(deprecated)]
    #[tokio::test]
    async fn legacy_subscriptions_report_changes() {
        use crate::tools::testutil::{run_git, write};
        use rmcp::model::{SubscribeRequestParams, UnsubscribeRequestParams};
        let (dir, _) = fixture_repo();
        let recorder = UpdateRecorder::default();
        let client = watching(dir.path(), recorder.clone(), false).await;
        let info = client.peer_info().unwrap();
        assert_eq!(
            info.capabilities
                .resources
                .as_ref()
                .and_then(|r| r.subscribe),
            Some(true)
        );
        for uri in ["gitrs://diff/index..worktree", "gitrs://commit/HEAD"] {
            client
                .subscribe(SubscribeRequestParams::new(uri))
                .await
                .unwrap();
        }
        let bad = client
            .subscribe(SubscribeRequestParams::new("gitrs://nope"))
            .await;
        assert!(bad.is_err());

        write(dir.path(), "a.txt", "edited\n");
        assert_eq!(recorder.wait_for(1).await, ["gitrs://diff/index..worktree"]);
        run_git(dir.path(), &["commit", "-q", "-am", "third"]);
        let seen = recorder.wait_for(3).await;
        assert!(seen.contains(&"gitrs://commit/HEAD".to_owned()), "{seen:?}");

        client
            .unsubscribe(UnsubscribeRequestParams::new("gitrs://commit/HEAD"))
            .await
            .unwrap();
        let before = recorder.0.lock().unwrap().len();
        run_git(
            dir.path(),
            &["commit", "-q", "--allow-empty", "-m", "fourth"],
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(recorder.0.lock().unwrap().len(), before, "unsubscribed");
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn listen_streams_report_changes() {
        use crate::tools::testutil::run_git;
        use rmcp::model::{ServerNotification, SubscriptionFilter};
        let (dir, _) = fixture_repo();
        let client = watching(dir.path(), (), true).await;
        let filter = SubscriptionFilter::builder()
            .resource_subscriptions(["gitrs://commit/HEAD".to_owned(), "nope".to_owned()])
            .build();
        let mut stream = client.listen(filter).await.unwrap();
        assert_eq!(
            stream.acknowledged().resource_subscriptions.as_deref(),
            Some(&["gitrs://commit/HEAD".to_owned()][..]),
            "only URIs that parse are accepted"
        );
        run_git(
            dir.path(),
            &["commit", "-q", "--allow-empty", "-m", "third"],
        );
        let note = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("an update arrives")
            .unwrap()
            .unwrap();
        match note {
            ServerNotification::ResourceUpdatedNotification(n) => {
                assert_eq!(n.params.uri, "gitrs://commit/HEAD");
            }
            other => panic!("unexpected notification {other:?}"),
        }

        // A second stream on another URI: each only hears about its own.
        let edits = SubscriptionFilter::builder()
            .resource_subscriptions([
                "gitrs://diff/index..worktree".to_owned(),
                "gitrs://commit/HEAD".to_owned(),
            ])
            .build();
        let mut both = client.listen(edits).await.unwrap();
        crate::tools::testutil::write(dir.path(), "a.txt", "edited\n");
        let note = tokio::time::timeout(std::time::Duration::from_secs(2), both.next())
            .await
            .expect("an update arrives")
            .unwrap()
            .unwrap();
        assert!(matches!(
            note,
            ServerNotification::ResourceUpdatedNotification(ref n)
                if n.params.uri == "gitrs://diff/index..worktree"
        ));
        let quiet =
            tokio::time::timeout(std::time::Duration::from_millis(100), stream.next()).await;
        assert!(
            quiet.is_err(),
            "the first stream didn't subscribe to the edit"
        );

        stream.cancel().await.unwrap();
        both.cancel().await.unwrap();
        // Nothing is subscribed now; the poller idles, then stops once the server is gone.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        client.cancel().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    }

    // See `legacy_subscriptions_report_changes`.
    #[allow(deprecated)]
    #[tokio::test]
    async fn subscriptions_need_watch() {
        use rmcp::model::SubscribeRequestParams;
        let (dir, _) = fixture_repo();
        let client = connect(dir.path(), false).await;
        let info = client.peer_info().unwrap();
        assert_eq!(
            info.capabilities
                .resources
                .as_ref()
                .and_then(|r| r.subscribe),
            None
        );
        let err = client
            .subscribe(SubscribeRequestParams::new("gitrs://commit/HEAD"))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("--watch"), "{err:?}");
        client.cancel().await.unwrap();
    }

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
        GitServer::new(
            RepoHandle::open_isolated(path),
            Policy {
                allow_write,
                watch_interval: None,
            },
        )
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
