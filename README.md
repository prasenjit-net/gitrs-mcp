# gitrs-mcp

A local-only Git [MCP](https://modelcontextprotocol.io) server written in Rust on top of [gitoxide](https://github.com/GitoxideLabs/gitoxide). It never touches the network, it is read-only unless you pass `--allow-write`, and every tool returns structured JSON.

## Usage

```sh
cargo build --release
./target/release/gitrs-mcp --repo /path/to/repo          # read-only
./target/release/gitrs-mcp --repo . --allow-write         # enables write tools (Phase 3+)
```

Register it with Claude Code:

```sh
claude mcp add gitrs -- /absolute/path/to/gitrs-mcp --repo /path/to/repo
```

Logs go to stderr. Set the log level with `GITRS_LOG=debug`.

## Tools

| Tool | Description |
|---|---|
| `repo_info` | Paths, HEAD, current branch, bare/shallow, in-progress operation |
| `log` | Paginated commit history from any revision |
| `show_commit` | Full metadata and message of one commit |
| `list_refs` | Branches, tags or all refs |

See [ROADMAP.md](ROADMAP.md) for what's next.

## Development

```sh
cargo test      # tests build fixture repos with the system `git`
cargo clippy --all-targets
```
