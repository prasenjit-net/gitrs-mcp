# Configuration

## Command line

```
gitrs-mcp [OPTIONS]

Options:
  -r, --repo <REPO>  Repository to serve. Any path inside a worktree works; the repo is discovered upward [default: .]
      --allow-write  Enable tools that modify the repository (stage, commit, branch, ...). Without this flag the server is strictly read-only
  -h, --help         Print help
  -V, --version      Print version
```

- `--repo` can point at the repository root, any directory inside its working tree, or a bare repository. The server serves exactly one repository per process. Tool path arguments are always relative to that repository's root, whichever directory you pass.
- `--allow-write` is accepted but has no effect yet: every current tool is read-only. Write tools (stage, commit, branch, and so on) are planned for Phase 3 of the [roadmap](../ROADMAP.md) and will only be registered when the flag is given.

The server speaks MCP over stdin and stdout, so it is started by an MCP client rather than run on its own.

## Environment

| Variable | Effect |
|---|---|
| `GITRS_LOG` | Log filter, in `tracing` `EnvFilter` syntax: a level such as `info` or `debug`, or per-module directives like `gitrs_mcp=debug,rmcp=info`. Logs always go to stderr, because stdout carries the protocol. Unset means only errors are logged. |

## Git configuration that affects results

The server reads the repository's git configuration (local, global and system) the way git does, and these settings change what tools return:

| Setting | Affects |
|---|---|
| `diff.renames` | Rename detection in `diff` (all modes) and `status`. On by default; `copies` also detects copies; `false` turns it off. |
| `status.renames` | Rename detection between HEAD and the index (`status`, `diff` with `staged`), taking precedence over `diff.renames`. |
| `status.showUntrackedFiles` | Whether `status` collapses untracked directories (`normal`, the default) or lists every file (`all`). |
| `diff.algorithm` | The line-diff algorithm for `diff` and `blame`. |
| `core.autocrlf`, `.gitattributes` filters | How working-tree files are converted before `diff` compares them with the index, as `git diff` does. |
| `.gitignore`, `core.excludesFile` | What `status` treats as ignored. |
| `branch.<name>.remote` / `.merge`, `remote.<name>.fetch` | The `upstream` reported by `status`. |
| `core.notesRef`, `notes.displayRef` | Which notes refs `notes` reads when no `ref` is given. |
| `submodule.<name>.url` / `.branch` in `.git/config` | Override `.gitmodules` in `list_submodules`. |

`config_get` reports these and every other setting, but redacts values that may be secrets (credentials, tokens, passwords, `http.extraHeader`, and passwords inside URLs).

Nothing here makes the server touch the network.

## Client setup

Each client starts its own server process. Point `--repo` at the repository you want the assistant to work with. With several repositories, register one server per repository under different names.

### Claude Code

```sh
claude mcp add gitrs -- gitrs-mcp --repo /path/to/repo
```

Add `--scope project` to share the configuration with your team through a `.mcp.json` file in the repository. From a clone of gitrs-mcp, `make mcp-add REPO=/path/to/repo` builds a release binary and registers it in one step. Check the registration with `claude mcp list`.

### Claude Desktop

Add the server to `claude_desktop_config.json` (on macOS, `~/Library/Application Support/Claude/claude_desktop_config.json`; on Windows, `%APPDATA%\Claude\claude_desktop_config.json`), then restart Claude Desktop:

```json
{
  "mcpServers": {
    "gitrs": {
      "command": "gitrs-mcp",
      "args": ["--repo", "/path/to/repo"],
      "env": { "GITRS_LOG": "info" }
    }
  }
}
```

Use the absolute path of the binary (for example `~/.cargo/bin/gitrs-mcp`, written out in full) if Claude Desktop can't find it on its `PATH`.

### Other MCP clients

Any client that can launch a stdio MCP server works. Configure it to run the `gitrs-mcp` binary with `--repo /path/to/repo` as arguments. For example, VS Code reads `.vscode/mcp.json`:

```json
{
  "servers": {
    "gitrs": {
      "type": "stdio",
      "command": "gitrs-mcp",
      "args": ["--repo", "${workspaceFolder}"]
    }
  }
}
```

### Trying it by hand

`make inspector REPO=/path/to/repo` opens the MCP Inspector in your browser, where you can list the tools, read their schemas, and call them with your own arguments.
