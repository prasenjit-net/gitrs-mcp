# Configuration

## Command line

```
gitrs-mcp [OPTIONS] [COMMAND]

Commands:
  setup  Register gitrs-mcp with AI agents (Claude Code, Claude Desktop, Cursor, VS Code, Codex CLI)

Options:
  -r, --repo <REPO>  Repository to serve. Any path inside a worktree works; the repo is discovered upward. Repeat to serve several; the first is the default when a call names none. Without it, the repository containing the working directory is served, or none when there isn't one (the client's roots can still add repositories)
      --allow-write  Enable tools that modify the repository (stage, commit, branch, ...). Without this flag the server is strictly read-only
      --watch        Let clients subscribe to gitrs:// resources and notify them when one changes (a commit, staging, an edit on disk). Subscribed resources are checked once a second
  -h, --help         Print help
  -V, --version      Print version
```

- `--repo` can point at the repository root, any directory inside its working tree, or a bare repository. Repeat it to serve several repositories from one process. The first is the default; the others are picked with the `repo` argument every tool and prompt takes (`?repo=<name>` on resources), using the names [`list_repos`](tools.md#list_repos) shows. Tool path arguments are always relative to the chosen repository's root, whichever directory you pass.
- **MCP roots.** When the client declares roots (most editors send the open workspace folders), the server also serves the repository at each `file://` root, and follows changes to the list. A repository is served only when its root is the declared root or lies inside it. A root inside a larger repository doesn't expose that repository. A `repo` argument can only name served repositories, or paths inside them.
- `--allow-write` registers the write tools (`create_branch`, `delete_branch`, `rename_branch`, `create_tag`, `stage`, `unstage`, `commit`, `reset`, `restore`, `switch`, `clean`, `stash_push`, `stash_pop`, `stash_drop`, `merge`, `cherry_pick`, `revert`, `rebase`, `continue_operation`, `abort_operation`, `worktree_add`, `worktree_remove`; see [tools.md](tools.md)). Without it they aren't listed at all, and the server can't change the repository. Write tools record a committer from `user.name` / `user.email` (or `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL`) and refuse to run without one; `GIT_COMMITTER_DATE` pins the recorded time, as with git.

- `--watch` lets clients subscribe to [`gitrs://` resources](resources.md#subscriptions) and sends `notifications/resources/updated` when one changes. Only subscribed resources are checked, once a second, so the flag costs nothing until a client subscribes. Without it, subscribing is refused.

- **Without `--repo`,** the server serves the repository that contains its working directory, if there is one. Outside any repository it still starts, and serves nothing until the client sends roots; until then every tool answers "no repository is being served: open a folder inside a git repository in your client, or add `--repo <path>` to this server's registration", and `list_repos` returns an empty list. An explicit `--repo` that isn't a repository is still a startup error.

The server speaks MCP over stdin and stdout, so it is started by an MCP client rather than run on its own. `gitrs-mcp setup` is the one command that is run by hand; see below.

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

## Registering with your agents: `gitrs-mcp setup`

`gitrs-mcp setup` writes the server into the configuration of the AI agents on your machine, so you don't edit each one's file by hand.

```sh
gitrs-mcp setup                      # on a terminal: pick from the agents it finds
gitrs-mcp setup --client cursor      # or name them: claude-code, claude-desktop, cursor, vscode, codex
gitrs-mcp setup --all --dry-run      # show what every detected agent would get, change nothing
```

| Agent | `--client` | Registered with | User file | Project file |
|---|---|---|---|---|
| Claude Code | `claude-code` | its own CLI: `claude mcp add-json` | kept by `claude` | `.mcp.json`, by `claude` |
| Claude Desktop | `claude-desktop` | edits `claude_desktop_config.json` (`mcpServers`) | macOS `~/Library/Application Support/Claude/`, Windows `%APPDATA%\Claude\` | none |
| Cursor | `cursor` | edits `mcp.json` (`mcpServers`) | `~/.cursor/mcp.json` | `.cursor/mcp.json` |
| VS Code | `vscode` | edits `mcp.json` (`servers`) | macOS `~/Library/Application Support/Code/User/`, Linux `~/.config/Code/User/`, Windows `%APPDATA%\Code\User\` | `.vscode/mcp.json` |
| Codex CLI | `codex` | edits `config.toml` (`[mcp_servers.gitrs]`) | `~/.codex/config.toml` | `.codex/config.toml` |

Claude Desktop has no documented Linux location, so setup doesn't support it there; use `--print` and the manual steps below.

### Options

| Option | Effect |
|---|---|
| `--client <agent>` | Register with this agent. Repeatable. |
| `--all` | Every agent found on this machine (with `--remove`: every agent setup knows). |
| `--scope user\|project` | `user` (default for installing) registers for you in every project. `project` writes the project's own file, to commit and share; it registers the bare command `gitrs-mcp`, so each teammate needs it on their `PATH`. With `--remove` and `--status`, the default is both. |
| `--repo <path>` | Repository for the server to serve. Repeatable. Made absolute at user scope, kept as given at project scope. |
| `--allow-write` | Register the write tools too. Off unless asked for here or answered "yes" at the prompt, which defaults to "no". |
| `--watch` | Register with `--watch`. |
| `--name <name>` | The entry's name (default `gitrs`). |
| `--dry-run` | Show the changes and make none. |
| `--yes` | Don't ask for confirmation. |
| `--force` | Replace an existing entry of the same name that differs. Without it setup refuses and shows both. |
| `--status` | Show where the server is registered, and flag entries whose command no longer exists (after an uninstall, say). |
| `--remove` | Remove the entry the way it was added. Alone, it removes it from every agent and scope where it is registered, without prompting for a choice; add `--client` to limit it, `--dry-run` to preview. |
| `--print` | Print the entry for one `--client` to add by hand. Changes nothing. |

On a terminal, setup finds the agents, shows a checklist, asks about write tools, lists exactly what will change and asks to confirm. Without a terminal it never asks: pass `--client` or `--all` (and `--yes` is implied).

### What gets registered

- **Command.** At user scope, the absolute path of the `gitrs-mcp` that ran setup, because apps started from the Dock or Start menu don't see your shell's `PATH`. A path inside a Homebrew `Cellar/<version>/` directory is replaced by the `bin` link in the same prefix, which `brew upgrade` keeps pointing at the current version. At project scope, the bare name `gitrs-mcp`.
- **Arguments.** None by default: the client's project folder or roots choose the repository. `--repo`, `--allow-write` and `--watch` add arguments.
- **Nothing else.** No secrets and no environment variables.

Claude Desktop starts servers outside any project, so give it a repository with `--repo <path>`.

### How files are changed

setup edits files that belong to other applications, so it is careful with them. See [security.md](security.md#what-setup-changes) for the guarantees. In short: a file is parsed, only the `gitrs` entry changes and other entries and their order stay, the old file is copied to `<file>.gitrs-backup-<time>` first, the new one replaces it atomically with the same permissions, and a file that can't be parsed is left alone. JSON files are rewritten with two-space indentation. A file with comments (VS Code allows them in `mcp.json`) is refused rather than stripped of them: use `--print` and add the entry yourself. TOML keeps its comments and layout.

### Example

Real output, on a machine whose home directory is `/tmp/demo/home` with Cursor and Codex CLI installed:

```
$ gitrs-mcp setup --all --dry-run --repo ~/code/app
Registering `gitrs` (user scope) as: /tmp/demo/bin/gitrs-mcp --repo /tmp/demo/home/code/app
  Cursor          add     /tmp/demo/home/.cursor/mcp.json
  Codex CLI       add     /tmp/demo/home/.codex/config.toml
Dry run: nothing was changed.

$ gitrs-mcp setup --all --yes
Registering `gitrs` (user scope) as: /tmp/demo/bin/gitrs-mcp
  Cursor          add     /tmp/demo/home/.cursor/mcp.json
  Codex CLI       add     /tmp/demo/home/.codex/config.toml
  Cursor          added `gitrs` in /tmp/demo/home/.cursor/mcp.json
    backup of the old file: /tmp/demo/home/.cursor/mcp.json.gitrs-backup-1793214007
  Codex CLI       added `gitrs` in /tmp/demo/home/.codex/config.toml
Cursor: Reload Cursor; the server shows under Settings > MCP.
Codex CLI: Start a new Codex session. A project's .codex/config.toml only applies once you trust the project.

$ gitrs-mcp setup --status
  Claude Code     not registered
  Claude Desktop  not registered
  Cursor          registered: /tmp/demo/bin/gitrs-mcp (/tmp/demo/home/.cursor/mcp.json)
  Cursor          (project) not registered
  VS Code         not registered
  VS Code         (project) not registered
  Codex CLI       registered: /tmp/demo/bin/gitrs-mcp (/tmp/demo/home/.codex/config.toml)
  Codex CLI       (project) not registered

$ gitrs-mcp setup --client cursor --remove --yes
  Cursor          remove  /tmp/demo/home/.cursor/mcp.json
  Cursor          (project) not registered
  Cursor          removed `gitrs` in /tmp/demo/home/.cursor/mcp.json
    backup of the old file: /tmp/demo/home/.cursor/mcp.json.gitrs-backup-1793214007-2

$ gitrs-mcp setup --print --client codex --allow-write
Add this to /tmp/demo/home/.codex/config.toml (format checked against Codex CLI 2026-10):
[mcp_servers.gitrs]
command = "/tmp/demo/bin/gitrs-mcp"
args = ["--allow-write"]
```

The backup and `--status` lines show what to look for after an uninstall: a registered command that no longer exists is marked, and `--remove` clears it.

## Roots and repositories

Clients that send their open workspace folders as MCP roots need no `--repo`: the server serves the repositories at those roots. Which clients send roots varies, and hasn't been checked for every client yet; if a client's tools answer "no repository is being served", add `--repo <path>` to its registration (`gitrs-mcp setup --repo <path>`).

## Client setup by hand

Prefer `gitrs-mcp setup` (above). These are the same registrations, for when you want to write them yourself or setup doesn't support your client.

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
