//! End-to-end tests: spawn the `gitrs-mcp` binary and talk to it over stdio with rmcp's client,
//! as an MCP client application would. Unit tests in `src/` cover behaviour in depth; these check
//! what only the real process shows: argument parsing, start-up from another directory, and the
//! stdio transport.
//!
//! This is a separate crate that can't reach `tools::testutil`, so it builds its small fixture here.

use std::{path::Path, process::Command};

use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, GetPromptRequestParams, ReadResourceRequestParams},
    service::RunningService,
    transport::TokioChildProcess,
};

const BIN: &str = env!("CARGO_BIN_EXE_gitrs-mcp");

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repository with two commits on `main` and a file in a subdirectory.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn one() {}\npub fn two() {}\n",
    )
    .unwrap();
    git(root, &["commit", "-q", "-am", "second"]);
    dir
}

/// Start the server in `cwd` with `args` and connect a client to it.
async fn spawn(cwd: &Path, args: &[&str]) -> RunningService<RoleClient, ()> {
    let mut command = tokio::process::Command::new(BIN);
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GITRS_LOG", "off");
    let transport = TokioChildProcess::new(command).unwrap();
    ().serve(transport)
        .await
        .expect("the server starts and initializes")
}

fn call(name: &str, arguments: serde_json::Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name.to_owned())
        .with_arguments(arguments.as_object().cloned().unwrap_or_default())
}

async fn structured<H: rmcp::ClientHandler>(
    client: &RunningService<RoleClient, H>,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let result = client.call_tool(call(name, arguments)).await.unwrap();
    assert_ne!(result.is_error, Some(true), "{name}: {result:?}");
    result.structured_content.unwrap()
}

#[tokio::test]
async fn a_read_only_session() {
    let repo = fixture();
    let client = spawn(repo.path(), &["--repo", "."]).await;
    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "gitrs-mcp");

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 28, "read-only tools only");
    assert!(
        tools
            .iter()
            .all(|t| t.annotations.as_ref().unwrap().read_only_hint == Some(true))
    );

    let log = structured(&client, "log", serde_json::json!({})).await;
    assert_eq!(log["commits"][0]["summary"], "second");
    let failed = client
        .call_tool(call("show_commit", serde_json::json!({"rev": "nope"})))
        .await
        .unwrap();
    assert_eq!(failed.is_error, Some(true));

    let read = client
        .read_resource(ReadResourceRequestParams::new(
            "gitrs://file/HEAD/src/lib.rs",
        ))
        .await
        .unwrap();
    let text = serde_json::to_value(&read.contents).unwrap()[0]["text"].clone();
    assert_eq!(text, "pub fn one() {}\npub fn two() {}\n");

    let mut arguments = serde_json::Map::new();
    arguments.insert("path".into(), "src/lib.rs".into());
    let prompt = client
        .get_prompt(GetPromptRequestParams::new("explain_history").with_arguments(arguments))
        .await
        .unwrap();
    assert_eq!(prompt.messages.len(), 1);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn allow_write_registers_the_write_tools() {
    let repo = fixture();
    let client = spawn(repo.path(), &["--repo", ".", "--allow-write"]).await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 50);
    let created = structured(
        &client,
        "create_branch",
        serde_json::json!({"name": "topic"}),
    )
    .await;
    assert_eq!(created["short_name"], "topic");
    client.cancel().await.unwrap();
}

/// Started from a subdirectory with a relative `--repo`, paths are still relative to the root.
#[tokio::test]
async fn paths_are_from_the_root_whatever_the_start_directory() {
    let repo = fixture();
    std::fs::write(repo.path().join("src/lib.rs"), "changed\n").unwrap();
    let client = spawn(&repo.path().join("src"), &["--repo", "."]).await;
    let status = structured(&client, "status", serde_json::json!({})).await;
    assert_eq!(status["unstaged"][0]["path"], "src/lib.rs");
    let diff = structured(
        &client,
        "diff",
        serde_json::json!({"pathspec": ["src/"], "format": "name_status"}),
    )
    .await;
    assert_eq!(diff["files"][0]["path"], "src/lib.rs");
    let found = structured(&client, "grep", serde_json::json!({"pattern": "changed"})).await;
    assert_eq!(found["matches"][0]["path"], "src/lib.rs");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn several_repositories() {
    let (first, second) = (fixture(), fixture());
    git(
        second.path(),
        &["commit", "-q", "--allow-empty", "-m", "third"],
    );
    let parent = first.path().parent().unwrap();
    let client = spawn(
        parent,
        &[
            "--repo",
            first.path().to_str().unwrap(),
            "--repo",
            second.path().to_str().unwrap(),
        ],
    )
    .await;
    let repos = structured(&client, "list_repos", serde_json::json!({})).await;
    let name = repos["repos"][1]["name"].as_str().unwrap().to_owned();
    let log = structured(
        &client,
        "log",
        serde_json::json!({"repo": name, "limit": 1}),
    )
    .await;
    assert_eq!(log["commits"][0]["summary"], "third");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn not_a_repository_fails_to_start() {
    let empty = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .args(["--repo", empty.path().to_str().unwrap()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CEILING_DIRECTORIES", empty.path().parent().unwrap())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no git repository found"), "{stderr}");
}

/// A client whose single MCP root is `.0`.
struct RootsClient(String);

// Roots are deprecated by protocol 2026-07-28; this client declares them like today's clients.
#[allow(deprecated)]
impl rmcp::ClientHandler for RootsClient {
    async fn list_roots(
        &self,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, rmcp::ErrorData> {
        Ok(rmcp::model::ListRootsResult::new(vec![
            rmcp::model::Root::new(&self.0),
        ]))
    }

    fn get_info(&self) -> rmcp::model::ClientConfig {
        rmcp::model::ClientConfig::new(
            rmcp::model::ClientCapabilities::builder()
                .enable_roots()
                .build(),
            rmcp::model::Implementation::new("roots-client", "0"),
        )
    }
}

/// The server's command, launched in `cwd` (outside every repository, which `cwd`'s parent seals).
fn command_outside_repositories(cwd: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(BIN);
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", cwd.parent().unwrap())
        .env("GITRS_LOG", "off");
    command
}

#[tokio::test]
async fn without_a_repository_the_server_starts_and_says_what_to_do() {
    let empty = tempfile::tempdir().unwrap();
    let transport = TokioChildProcess::new(command_outside_repositories(empty.path())).unwrap();
    let client = ().serve(transport).await.expect("starts with no repository");

    let repos = structured(&client, "list_repos", serde_json::json!({})).await;
    assert_eq!(repos["repos"].as_array().unwrap().len(), 0);
    let refused = client
        .call_tool(call("status", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    let message = serde_json::to_string(&refused.content).unwrap();
    assert!(message.contains("--repo <path>"), "{message}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn without_a_repository_the_clients_roots_are_served() {
    let repo = fixture();
    let launched_in = tempfile::tempdir().unwrap();
    let transport =
        TokioChildProcess::new(command_outside_repositories(launched_in.path())).unwrap();
    let root = format!("file://{}", repo.path().canonicalize().unwrap().display());
    let client = RootsClient(root)
        .serve(transport)
        .await
        .expect("starts with no repository");

    // The server asks for the roots once the handshake is done.
    for _ in 0..200 {
        let repos = client
            .call_tool(call("list_repos", serde_json::json!({})))
            .await
            .unwrap();
        if repos.structured_content.unwrap()["repos"]
            .as_array()
            .is_some_and(|r| !r.is_empty())
        {
            let log = structured(&client, "log", serde_json::json!({"limit": 1})).await;
            assert_eq!(log["commits"][0]["summary"], "second");
            client.cancel().await.unwrap();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the root was never served");
}
