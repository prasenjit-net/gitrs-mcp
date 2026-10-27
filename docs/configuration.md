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
- `--allow-write` registers the write tools (`create_branch`, `delete_branch`, `rename_branch`, `create_tag`, `stage`, `unstage`, `commit`, `reset`, `restore`, `switch`, `clean`, `stash_push`, `stash_pop`, `stash_drop`; see Phases 3 and 4 of the [roadmap](../ROADMAP.md)). Without it they aren't listed at all, and the server can't change the repository. Write tools record a committer from `user.name` / `user.email` (or `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`) and refuse to run without one; `GIT_COMMITTER_DATE` pins the recorded time, as with git.

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
| `.gitignore`, `.git/info/exclude`, `core.excludesFile` | What `status` treats as ignored, and the rules `check_ignore` reports. |
| `.gitattributes`, `.git/info/attributes`, `core.attributesFile` | What `check_attr` reports, and the filters and line-ending conversion `stage` applies. |
| `core.fileMode` | Whether `stage` records the executable bit from disk (`true`, the default) or keeps the index's (`false`). |
| `core.autocrlf`, `core.safecrlf` | Line-ending conversion when `stage` hashes files, as with `git add`. |
| `user.name`, `user.email` (and `author.*`, `committer.*`) | The identity `commit`, `create_tag` and reflog entries record. Required by write tools. |
| `commit.gpgSign`, `tag.gpgSign` | When `true`, `commit` and annotated `create_tag` refuse, because gitrs-mcp can't sign. |
| `.gitattributes` filters, `core.autocrlf`, `core.eol` | How `restore` and `switch` write files to the working tree, as `git checkout` does. |
| `core.logAllRefUpdates` | Whether write tools add reflog entries (default `true` in a repository with a working tree). |
| `core.hooksPath` | Where `commit` looks for the hooks it reports in `hooks_skipped` (default `.git/hooks`). Hooks are never run. |
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
