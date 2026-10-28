//! `gitrs-mcp`: a local-only Git MCP server built on gitoxide (`gix`).
//!
//! Speaks MCP over stdio. Never opens a network connection: gix is compiled
//! without any transport features, so fetch/push/clone are impossible by construction.

use std::path::PathBuf;

use clap::Parser;
use gitrs_mcp::{GitServer, Policy, RepoHandle};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about = "Local-only Git MCP server (stdio)")]
struct Args {
    /// Repository to serve. Any path inside a worktree works; the repo is discovered upward.
    /// Repeat to serve several; the first is the default when a call names none.
    #[arg(short, long, default_value = ".")]
    repo: Vec<PathBuf>,

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
    // Find every root before leaving the launch directory, since `--repo` may be relative to it.
    let roots = args
        .repo
        .iter()
        .map(|path| RepoHandle::find_root(path))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if let Some(first) = roots.first() {
        RepoHandle::leave_worktrees(first)?;
    }
    let repos = roots
        .into_iter()
        .map(RepoHandle::open)
        .collect::<anyhow::Result<Vec<_>>>()?;
    for repo in &repos {
        tracing::info!(git_dir = %repo.git_dir().display(), "serving");
    }
    tracing::info!(
        allow_write = args.allow_write,
        watch = args.watch,
        "starting"
    );

    let policy = Policy {
        allow_write: args.allow_write,
        watch_interval: args.watch.then_some(std::time::Duration::from_secs(1)),
    };
    let service = GitServer::serving(repos, policy).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
