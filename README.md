# gitrs-mcp

A local-only Git [MCP](https://modelcontextprotocol.io) server written in Rust, built on [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`) and the official Rust MCP SDK ([`rmcp`](https://crates.io/crates/rmcp)).

It lets AI assistants inspect a Git repository through typed tools with structured JSON output. It doesn't shell out to `git` and doesn't parse porcelain text.

- **Local only, by construction.** gix is compiled without any network features, so fetch, push and clone can't happen.
- **Read-only by default.** Write tools (coming in Phase 3) will exist only when you start the server with `--allow-write`.
- **Structured output.** Every tool returns JSON with a published output schema. Bad input, such as an unknown revision, comes back as a tool error the model can read and correct.
- **Pure Rust.** It runs as a single static binary that speaks MCP over stdio, and it doesn't need `libgit2` or the `git` executable.

> **Status:** early development, at Phase 0 of the [roadmap](ROADMAP.md). The current tools are read-only history and ref inspection.

## Install

You need Rust 1.85+ (edition 2024).

```sh
# straight from GitHub
cargo install --git https://github.com/prasenjit-net/gitrs-mcp.git

# or from a clone
git clone https://github.com/prasenjit-net/gitrs-mcp.git
cd gitrs-mcp
make install          # cargo install --path . --locked
```

## Usage

```
gitrs-mcp [OPTIONS]

Options:
  -r, --repo <REPO>  Repository to serve; any path inside a worktree works (default: .)
      --allow-write  Enable tools that modify the repository
  -h, --help         Print help
  -V, --version      Print version
```

The server talks MCP over stdin/stdout and logs to stderr. Set the log level with `GITRS_LOG` (for example `GITRS_LOG=debug`).

### Claude Code

```sh
claude mcp add gitrs -- gitrs-mcp --repo /path/to/repo
```

From a clone, `make mcp-add REPO=/path/to/repo` builds a release binary and registers it.

### Claude Desktop and other MCP clients

```json
{
  "mcpServers": {
    "gitrs": {
      "command": "gitrs-mcp",
      "args": ["--repo", "/path/to/repo"]
    }
  }
}
```

## Tools

Revision arguments accept anything `git rev-parse` does: SHAs and SHA prefixes, branches, tags, `HEAD~2`, `v1.0^{commit}`, and so on.

| Tool | Arguments | Returns |
|---|---|---|
| `repo_info` | none | git dir, worktree, bare/shallow flags, current branch, HEAD commit, in-progress operation (merge, rebase, …) |
| `log` | `rev?` (default `HEAD`), `limit?` (default 20, max 500), `skip?`, `first_parent?` | Commits newest first (id, short id, summary, author, parents) plus a `truncated` flag for pagination |
| `show_commit` | `rev?` (default `HEAD`) | Full commit: tree, parents, author, committer, summary, full message |
| `list_refs` | `kind?`: `branches` (default), `tags`, `all` | Refs with full and short names, peeled target commit, and which branch HEAD is on |

The next phases add `status`, `diff`, `read_file`, `blame`, `grep`, safe write tools behind `--allow-write`, and more. The full plan is in [ROADMAP.md](ROADMAP.md).

## Development

```sh
make            # list all targets
make test       # unit tests (fixture repos are built with the system `git`)
make smoke      # end-to-end MCP round trip over stdio against REPO (default .)
make inspector  # explore the tools interactively in the MCP Inspector (needs npx)
make check      # fmt-check + clippy + test + smoke: what CI should run
make coverage   # coverage summary, fails below 80% lines (needs `cargo install cargo-llvm-cov`)
```

Contributor rules (code quality, MCP documentation, testing and the pre-commit checklist) are in [CLAUDE.md](CLAUDE.md).

Every target accepts `REPO=<path>`, `ARGS="<extra server flags>"` and `LOG=<level>`.

### Layout

```
src/
├─ main.rs        CLI arguments, logging, stdio transport
├─ server.rs      MCP tool registration (thin async wrappers)
├─ repo.rs        Thread-safe repo handle; runs gix calls on the blocking pool
└─ tools/         Git logic as plain sync functions over &gix::Repository
   ├─ repo.rs     repo_info
   ├─ history.rs  log, show_commit
   └─ refs.rs     list_refs
scripts/smoke.sh  stdio MCP smoke test
```

To add a tool:
1. Write a sync function in `tools/<area>.rs` that takes a `Deserialize + JsonSchema` args struct and returns a `Serialize + JsonSchema` result. Doc comments on the fields become the schema descriptions the model sees.
2. Add a few-line `#[tool]` wrapper in `server.rs`.
3. Write tests that compare the result with real `git` on a fixture repo.
