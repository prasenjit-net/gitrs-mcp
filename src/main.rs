//! `gitrs-mcp`: a local-only Git MCP server built on gitoxide (`gix`).
//!
//! Speaks MCP over stdio. Never opens a network connection: gix is compiled
//! without any transport features, so fetch/push/clone are impossible by construction.

mod repo;
mod server;
mod tools;

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

use crate::{
    repo::RepoHandle,
    server::{GitServer, Policy},
};

#[derive(Debug, Parser)]
#[command(version, about = "Local-only Git MCP server (stdio)")]
struct Args {
    /// Repository to serve. Any path inside a worktree works; the repo is discovered upward.
    #[arg(short, long, default_value = ".")]
    repo: PathBuf,

    /// Enable tools that modify the repository (stage, commit, branch, ...).
    /// Without this flag the server is strictly read-only.
    #[arg(long)]
    allow_write: bool,

    /// Let clients subscribe to gitrs:// resources and notify them when one changes (a commit,
    /// staging, an edit on disk). Subscribed resources are checked once a second.
    #[arg(long)]
    watch: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout carries the MCP protocol, so all logging must go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("GITRS_LOG"))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = Args::parse();
    let repo = RepoHandle::discover(&args.repo).with_context(|| {
        format!(
            "no git repository found at or above {}",
            args.repo.display()
        )
    })?;
    tracing::info!(git_dir = %repo.git_dir().display(), allow_write = args.allow_write, watch = args.watch, "starting");

    let policy = Policy {
        allow_write: args.allow_write,
        watch_interval: args.watch.then_some(std::time::Duration::from_secs(1)),
    };
    let service = GitServer::new(repo, policy).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
