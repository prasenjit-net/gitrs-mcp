# Configuration

## Command line

```
gitrs-mcp [OPTIONS]

Options:
  -r, --repo <REPO>  Repository to serve. Any path inside a worktree works; the repo is discovered upward. Repeat to serve several; the first is the default when a call names none [default: .]
      --allow-write  Enable tools that modify the repository (stage, commit, branch, ...). Without this flag the server is strictly read-only
      --watch        Let clients subscribe to gitrs:// resources and notify them when one changes (a commit, staging, an edit on disk). Subscribed resources are checked once a second
  -h, --help         Print help
  -V, --version      Print version
```

- `--repo` can point at the repository root, any directory inside its working tree, or a bare repository. Repeat it to serve several repositories from one process. The first is the default; the others are picked with the `repo` argument every tool and prompt takes (`?repo=<name>` on resources), using the names [`list_repos`](tools.md#list_repos) shows. Tool path arguments are always relative to the chosen repository's root, whichever directory you pass.
- **MCP roots.** When the client declares roots (most editors send the open workspace folders), the server also serves the repository at each `file://` root, and follows changes to the list. A repository is served only when its root is the declared root or lies inside it. A root inside a larger repository doesn't expose that repository. A `repo` argument can only name served repositories, or paths inside them.
- `--allow-write` registers the write tools (`create_branch`, `delete_branch`, `rename_branch`, `create_tag`, `stage`, `unstage`, `commit`, `reset`, `restore`, `switch`, `clean`, `stash_push`, `stash_pop`, `stash_drop`, `merge`, `cherry_pick`, `revert`, `rebase`, `continue_operation`, `abort_operation`, `worktree_add`, `worktree_remove`; see Phases 3 and 4 of the [roadmap](../ROADMAP.md)). Without it they aren't listed at all, and the server can't change the repository. Write tools record a committer from `user.name` / `user.email` (or `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`) and refuse to run without one; `GIT_COMMITTER_DATE` pins the recorded time, as with git.

- `--watch` lets clients subscribe to [`gitrs://` resources](resources.md#subscriptions) and sends `notifications/resources/updated` when one changes. Only subscribed resources are checked, once a second, so the flag costs nothing until a client subscribes. Without it, subscribing is refused.

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

Each client starts its own server process over stdio. Point `--repo` at the repository the assistant should work with. Repeat it to serve several, or rely on the client's roots: clients that send their open workspace folders as MCP roots get those repositories served automatically. If the client can't find `gitrs-mcp` on its `PATH`, use the binary's absolute path, for example `/Users/you/.cargo/bin/gitrs-mcp`.

### Claude Code

```sh
claude mcp add gitrs -- gitrs-mcp --repo /path/to/repo
```

Add `--scope project` to share the configuration with your team through a `.mcp.json` file in the repository. Add `--allow-write` after `--repo` to enable the write tools, and `--watch` for resource change notifications. From a clone of gitrs-mcp, `make mcp-add REPO=/path/to/repo` builds a release binary and registers it in one step. Check the registration with `claude mcp list`.

### Claude Desktop

Add the server to `claude_desktop_config.json`, then restart Claude Desktop. The file is at `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS, and `%APPDATA%\Claude\claude_desktop_config.json` on Windows.

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

### VS Code

Add `.vscode/mcp.json` to the workspace (or run **MCP: Add Server** from the command palette). `${workspaceFolder}` serves the open folder:

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

### Cursor

Add `.cursor/mcp.json` to the project, or `~/.cursor/mcp.json` for every project:

```json
{
  "mcpServers": {
    "gitrs": {
      "command": "gitrs-mcp",
      "args": ["--repo", "${workspaceFolder}"]
    }
  }
}
```

### Other MCP clients

Any client that can launch a stdio MCP server works. Configure it to run `gitrs-mcp` with `--repo /path/to/repo` as arguments.

### Trying it by hand

`make inspector REPO=/path/to/repo` opens the MCP Inspector in your browser, where you can list the tools, read their schemas, and call them with your own arguments.
