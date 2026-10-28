//! `gitrs-mcp`: a local-only Git MCP server built on gitoxide (`gix`).
//!
//! Speaks MCP over stdio. Never opens a network connection: gix is compiled
//! without any transport features, so fetch/push/clone are impossible by construction.

use std::path::{Path, PathBuf};

use clap::Parser;
use gitrs_mcp::{GitServer, Policy, RepoHandle, setup};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[derive(Debug, clap::Subcommand)]
enum Command {
    Setup(setup::SetupArgs),
}

#[derive(Debug, Parser)]
#[command(version, about = "Local-only Git MCP server (stdio)")]
struct Args {
    /// Without a command, run the server.
    #[command(subcommand)]
    command: Option<Command>,

    /// Repository to serve. Any path inside a worktree works; the repo is discovered upward.
    /// Repeat to serve several; the first is the default when a call names none. Without it, the
    /// repository containing the working directory is served, or none when there isn't one (the
    /// client's roots can still add repositories).
    #[arg(short, long)]
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

/// The repositories to serve from the command line. A `--repo` that isn't a repository is an error
/// worth reporting; the implicit working directory not being one is not, since a user-level
/// registration launches the server wherever the client happens to be.
fn launch_roots(requested: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    if requested.is_empty() {
        return Ok(RepoHandle::find_root(Path::new("."))
            .map(|root| vec![root])
            .unwrap_or_default());
    }
    requested
        .iter()
        .map(|path| RepoHandle::find_root(path))
        .collect()
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
    if let Some(Command::Setup(setup_args)) = args.command {
        return setup::run(setup_args);
    }
    // Find every root before leaving the launch directory, since `--repo` may be relative to it.
    let roots = launch_roots(&args.repo)?;
    let leave_from = match roots.first() {
        Some(first) => first.clone(),
        None => std::env::current_dir()?,
    };
    RepoHandle::leave_worktrees(&leave_from)?;
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
