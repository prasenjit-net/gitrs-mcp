//! `gitrs-mcp setup` as a user runs it: the real binary, with `HOME` pointed at a temporary
//! directory so nothing outside it is touched. The point is to prove that what setup registers
//! actually starts.

use std::{path::Path, process::Command};

use rmcp::{ServiceExt, transport::TokioChildProcess};

const BIN: &str = env!("CARGO_BIN_EXE_gitrs-mcp");

fn setup(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .arg("setup")
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap()
}

#[tokio::test]
async fn what_setup_registers_starts_and_serves_tools() {
    let home = tempfile::tempdir().unwrap();
    let output = setup(home.path(), &["--client", "cursor", "--yes"]);
    assert!(output.status.success(), "{output:?}");

    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".cursor/mcp.json")).unwrap())
            .unwrap();
    let entry = &config["mcpServers"]["gitrs"];
    assert_eq!(entry["type"], "stdio");

    // Launch it as Cursor would: the registered command and arguments, nothing else.
    let mut command = tokio::process::Command::new(entry["command"].as_str().unwrap());
    command
        .args(
            entry["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap()),
        )
        .current_dir(home.path())
        .env("GITRS_LOG", "off")
        .env("GIT_CEILING_DIRECTORIES", home.path().parent().unwrap());
    let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(
        tools.len(),
        28,
        "read-only unless --allow-write was asked for"
    );
    client.cancel().await.unwrap();
}

#[test]
fn status_and_remove_round_trip() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        setup(home.path(), &["--client", "codex", "--yes"])
            .status
            .success()
    );

    let status = setup(home.path(), &["--status", "--client", "codex"]);
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.status.success()
            && text.contains("registered:")
            && !text.contains("no longer exists"),
        "{text}"
    );

    assert!(
        setup(home.path(), &["--client", "codex", "--remove", "--yes"])
            .status
            .success()
    );
    let status = setup(home.path(), &["--status", "--client", "codex"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("not registered"));
}

#[test]
fn setup_without_agents_to_choose_says_what_to_pass() {
    let home = tempfile::tempdir().unwrap();
    let output = setup(home.path(), &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--client") && stderr.contains("--all"),
        "{stderr}"
    );
}
