# gitrs-mcp

A local-only Git [MCP](https://modelcontextprotocol.io) server written in Rust, built on [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`) and the official Rust MCP SDK ([`rmcp`](https://crates.io/crates/rmcp)).

It lets AI assistants inspect a Git repository through typed tools with structured JSON output. It doesn't shell out to `git` and doesn't parse porcelain text.

- **Local only, by construction.** gix is compiled without any network features, so fetch, push and clone can't happen.
- **Read-only by default.** Write tools exist only when you start the server with `--allow-write`; without it they aren't even listed. They check everything before writing, refuse instead of overwriting work, and default to a dry run when data could be lost.
- **Git-compatible.** Results match the `git` CLI on the same repository: the test suite checks each tool against real `git` output, and generated patches apply with `git apply`.
- **Structured output.** Every tool returns JSON with a published output schema. Bad input, such as an unknown revision, comes back as a tool error the model can read and correct.
- **Several repositories.** Repeat `--repo`, or let the client's MCP roots add its open projects. Every tool, prompt and resource takes an optional `repo` to pick one.
- **Context to attach.** Commits, files and diffs are also served as `gitrs://` resources (for example the staged changes), with completion of revisions and paths. Prompts such as `write_commit_message` and `review_changes` come filled in with the diff they need.
- **Pure Rust.** It runs as a single static binary that speaks MCP over stdio, and it doesn't need `libgit2` or the `git` executable.

> **Status:** all six phases of the [roadmap](ROADMAP.md) are done, and 0.1.0 is ready to release: 28 read-only tools covering status, diffs, merge previews and conflicts, file contents, code search, ignore and attribute rules, history, blame, refs, tags, reflogs, stashes, notes, worktrees, submodules, config and object stats; and, with `--allow-write`, 22 write tools for the local edit, stage, commit and branch loop, plus merges, cherry-picks, reverts, rebases, stashes, hard resets, `clean` and worktrees. Merges and rebases that stop on conflicts leave git's own state, so you can finish them with either gitrs-mcp or git. On top of the tools come `gitrs://` resources, prompts, completion, progress and cancellation, change notifications (`--watch`), and several repositories per server.

## Install

Once the first release is published, prebuilt binaries for macOS, Linux and Windows are attached to each [GitHub release](https://github.com/prasenjit-net/gitrs-mcp/releases), with installers:

```sh
# macOS and Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/prasenjit-net/gitrs-mcp/releases/latest/download/gitrs-mcp-installer.sh | sh

# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/prasenjit-net/gitrs-mcp/releases/latest/download/gitrs-mcp-installer.ps1 | iex"

# Homebrew
brew install prasenjit-net/tap/gitrs-mcp

# crates.io (needs Rust 1.89+)
cargo install gitrs-mcp
```

Until then, build it from source with Rust 1.89 or newer:

```sh
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
  -r, --repo <REPO>  Repository to serve; any path inside a worktree works (default: .).
                     Repeat to serve several; the first is the default
      --allow-write  Enable tools that modify the repository
      --watch        Notify clients when subscribed gitrs:// resources change
  -h, --help         Print help
  -V, --version      Print version
```

The server talks MCP over stdin/stdout and logs to stderr. Set the log level with `GITRS_LOG` (for example `GITRS_LOG=debug`).

### Claude Code

```sh
claude mcp add gitrs -- gitrs-mcp --repo /path/to/repo
```

From a clone, `make mcp-add REPO=/path/to/repo` builds a release binary and registers it.

### Claude Desktop, VS Code, Cursor and other MCP clients

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

Copy-paste setup for Claude Code, Claude Desktop, VS Code and Cursor, logging, and the git settings that affect results are in [docs/configuration.md](docs/configuration.md).

## Tools

Revision arguments accept anything `git rev-parse` does: SHAs and SHA prefixes, branches, tags, `HEAD~2`, `v1.0^{commit}`, and so on. Paths and pathspecs are relative to the repository root. Every list is paginated and reports `truncated`, and file contents and patches stay within a byte budget (64 KiB by default).

| Tool | Arguments | Returns |
|---|---|---|
| `status` | `pathspec?`, `include_untracked?` (default true), `include_ignored?`, `limit?` | Branch, HEAD, configured upstream, and staged, unstaged, untracked, ignored and conflicted paths, with full counts |
| `diff` | `from?`, `to?`, `staged?`, `pathspec?`, `format?`: `stat` (default), `name_status`, `patch`; `context_lines?`, `max_bytes?`, `limit?`, `skip?` | Changed files with status, line counts and git-compatible patches. Compares worktree↔index (default), index↔revision (`staged`) or two revisions (`from` + `to`) |
| `conflicts` | `include_content?`, `max_bytes?`, `limit?`, `skip?` | Conflicted files during a merge, rebase or cherry-pick: conflict kind and base/ours/theirs versions |
| `merge_preview` | `theirs`, `ours?` (default `HEAD`), `allow_unrelated?`, `include_content?`, `max_bytes?`, `limit?`, `skip?` | In-memory merge like `git merge-tree`: clean/up-to-date/fast-forward, merge bases, result tree, files that would conflict |
| `read_file` | `path`, `rev?` (default `HEAD`; or `index`, `worktree`), `offset_bytes?`, `max_bytes?` | File content as UTF-8 or base64, with size, mode, blob id, and `next_offset` for chunked reads |
| `list_tree` | `rev?`, `path?`, `recursive?`, `limit?`, `skip?` | Tree entries (path, kind, mode, id, size) in `git ls-tree` order |
| `blame` | `path`, `rev?`, `line_range?`, `limit?` | Hunks of lines with the commit, author and summary that last changed them, following renames |
| `check_ignore` | `paths` | Per path: ignored or not, tracked, and the deciding rule with its file and line |
| `check_attr` | `paths`, `attrs?` (default: all set attributes) | Per path: attribute states (`set`, `unset`, a value, `unspecified`) from `.gitattributes` |
| `grep` | `pattern`, `rev?` (default `worktree`; or `index`, a revision), `pathspec?`, `ignore_case?`, `fixed_strings?`, `limit?`, `skip?` | Matching lines (path, line, column, text) in tracked files, like `git grep` |
| `log` | `rev?` (default `HEAD`), `limit?` (default 20, max 500), `skip?`, `first_parent?` | Commits newest first (id, short id, summary, author, parents) |
| `show_commit` | `rev?` (default `HEAD`) | Full commit: tree, parents, author, committer, summary, full message |
| `file_history` | `path`, `rev?`, `limit?`, `skip?`, `follow_renames?` | Commits that changed a file or directory, like `git log -- <path>`, with how each changed it |
| `search_commits` | `message_regex?`, `author?`, `since?`, `until?`, `path?`, `rev?`, `limit?`, `skip?` | Commits matching every given filter, newest first |
| `rev_parse` | `spec` | Full SHAs and object type for a revision or a range (`a..b`, `a...b` with merge base) |
| `compare_refs` | `base`, `head`, `limit?` | Merge base, ahead/behind counts, and the newest commits on each side |
| `list_refs` | `kind?`: `branches` (default), `tags`, `all` | Refs with full and short names, peeled target commit, and which branch HEAD is on |
| `describe` | `rev?`, `tags?`, `long?`, `first_parent?` | `git describe --always` name such as `v1.2-3-gabc1234`, with the tag, depth and exact-match flag |
| `show_tag` | `name` | Annotated or lightweight, target and peeled commit, tagger, message, signed flag |
| `reflog` | `ref?` (default `HEAD`), `limit?`, `skip?` | Where the ref pointed over time: `ref@{n}` selector, old and new commit, committer, message |
| `list_stashes` | `limit?`, `skip?` | Stashes newest first: `stash@{n}`, commit, base commit, branch, message, untracked flag |
| `notes` | `rev?`, `ref?`, `max_bytes?` | Notes attached to a commit or object, per notes ref |
| `list_repos` | none | The served repositories (from `--repo` and the client's MCP roots) and their names for the `repo` argument |
| `repo_info` | none | git dir, worktree, bare/shallow flags, current branch, HEAD commit, in-progress operation (merge, rebase, …) |
| `list_worktrees` | `limit?`, `skip?` | Main and linked worktrees: path, HEAD, branch, locked/prunable, which one is served |
| `list_submodules` | `limit?`, `skip?` | Submodules with URL (passwords redacted), recorded and checked-out commits, and state, like `git submodule status` |
| `config_get` | `key?` or `section?`, `scope?`, `limit?`, `skip?` | Config values with scope and file; credentials, tokens, passwords and auth headers redacted |
| `count_objects` | none | Loose and packed object counts and sizes, packs, commit-graph and multi-pack-index presence |

With `--allow-write`, these write tools are added:

| Tool | Arguments | Does |
|---|---|---|
| `create_branch` | `name`, `start_point?` (default `HEAD`), `force?` | Creates a branch, like `git branch`; HEAD doesn't move |
| `delete_branch` | `name`, `force?`, `dry_run?` (default **true**) | Deletes a branch that isn't checked out and is merged (or any with `force`), like `git branch -d` |
| `rename_branch` | `old?` (default current), `new`, `force?` | Renames a branch with its reflog and config; HEAD follows |
| `create_tag` | `name`, `target?`, `message?` (annotated when set), `force?` | Creates a lightweight or annotated tag |
| `stage` | `paths` or `all` | Stages modifications, deletions and new files like `git add` (`-A` with `all`); resolves conflicts |
| `unstage` | `paths` or `all` | Resets index entries to HEAD like `git restore --staged`; the worktree is untouched |
| `commit` | `message`, `amend?`, `allow_empty?`, `author?` | Commits the index like `git commit -m`; reports hooks it didn't run; refuses when signing is required |
| `reset` | `rev?` (default `HEAD`), `mode?`: `mixed` (default), `soft` or `hard`; `dry_run?` (default **true** for `hard`) | Moves the branch like `git reset`; `hard` also overwrites the worktree and reports what would be lost; returns `previous` to undo |
| `restore` | `paths`, `source?`, `staged?`, `worktree?`, `dry_run?` (default **true**) | Restores files from the index or a revision like `git restore`; flags content that would be lost |
| `switch` | `target`, `create?`, `start_point?`, `detach?` | Switches branch like `git switch`, carrying local changes over and refusing to overwrite any |
| `clean` | `paths?`, `directories?`, `ignored?` (`keep`, `also`, `only`), `dry_run?` (default **true**) | Deletes untracked (and optionally ignored) files like `git clean -f`; never nested repositories |
| `stash_push` | `message?`, `include_untracked?` | Stashes local changes in git's stash format and resets to HEAD |
| `stash_pop` | `index?`, `restore_index?`, `drop?` (default true) | Applies a stash like `git stash pop`; refuses instead of conflicting |
| `stash_drop` | `index?`, `dry_run?` (default **true**) | Drops a stash like `git stash drop` |
| `merge` | `theirs`, `message?`, `fast_forward?` (`allow`, `never`, `only`) | Merges like `git merge`; stops on conflicts with git's merge state |
| `cherry_pick` | `rev` | Applies one commit's change as a new commit, like `git cherry-pick` |
| `revert` | `rev` | Commits the inverse of one commit, like `git revert` |
| `rebase` | `onto`, `upstream?`, `todo?` (`pick`, `reword`, `squash`, `fixup`, `drop`) | Replays the branch onto `onto` like `git rebase`, or follows a structured interactive plan |
| `continue_operation` | `message?`, `skip?` | Concludes a merge, cherry-pick, revert or rebase once conflicts are staged; also continues ones git started |
| `abort_operation` | none | Abandons the operation in progress, like `git merge --abort` / `git rebase --abort` |
| `worktree_add` | `path`, `target`, `create?`, `start_point?`, `detach?` | Adds a linked worktree like `git worktree add` |
| `worktree_remove` | `worktree`, `force?`, `dry_run?` (default **true**) | Removes a linked worktree like `git worktree remove`; its branch is kept |

The full reference, with every argument, result field, error and a real example per tool, is in [docs/tools.md](docs/tools.md). What's next is in [ROADMAP.md](ROADMAP.md).

## Documentation

- [docs/tools.md](docs/tools.md): tool reference.
- [docs/versioning.md](docs/versioning.md): what counts as a breaking change, and how the API snapshot enforces it.
- [docs/security.md](docs/security.md): threat model, guarantees and the tests behind them.
- [docs/resources.md](docs/resources.md): `gitrs://` resources, prompts and argument completion.
- [docs/configuration.md](docs/configuration.md): CLI flags, environment, git settings that affect results, and client setup.
- [docs/architecture.md](docs/architecture.md): layers, data flow, the sync/async bridge, and the safety model.
- [docs/development.md](docs/development.md): setup, make targets, adding a tool, testing and coverage.

## Development

```sh
make            # list all targets
make test       # unit tests (fixture repos are built with the system `git`)
make smoke      # end-to-end MCP round trip over stdio, calling every tool against REPO (default .)
make inspector  # explore the tools interactively in the MCP Inspector (needs npx)
make check      # fmt-check + clippy + test + smoke: what CI should run
make coverage   # coverage summary, fails below 80% lines (needs `cargo install cargo-llvm-cov`)
make bench      # time tool calls against git on REPO (try a clone of git.git)
make stress     # heaviest calls at their largest limits on REPO; checks output stays bounded
make fuzz       # run the cargo-fuzz targets (needs nightly and cargo-fuzz)
```

Every target accepts `REPO=<path>`, `ARGS="<extra server flags>"` and `LOG=<level>`. `make smoke` also takes `FILE=<tracked file>` (default `README.md`) for the tools that need a path.

Contributor rules (code quality, MCP documentation, testing and the pre-commit checklist) are in [CLAUDE.md](CLAUDE.md), and the step-by-step guide to adding a tool is in [docs/development.md](docs/development.md).

### Layout

```
src/
├─ main.rs          CLI arguments, logging, stdio transport
├─ server/          MCP registration: thin async wrappers, one router per area, resources, prompts, completion, and the served repositories
├─ repo.rs          Thread-safe repository handles; runs gix calls on the blocking pool
└─ tools/           Git logic as plain sync functions over &gix::Repository
   ├─ mod.rs        shared types and helpers, test fixtures
   ├─ repo.rs       repo_info
   ├─ resources.rs  gitrs:// resources (commit, file, diff) and argument completion
   ├─ refs.rs       list_refs, describe, show_tag
   ├─ reflog.rs     reflog, list_stashes
   ├─ notes.rs      notes
   ├─ revision.rs   rev_parse, compare_refs
   ├─ status.rs     status
   ├─ diff.rs       diff
   ├─ merge.rs      conflicts, merge_preview
   ├─ files.rs      read_file, list_tree
   ├─ grep.rs       grep
   ├─ attributes.rs check_ignore, check_attr
   ├─ blame.rs      blame
   ├─ worktrees.rs  list_worktrees, worktree_add, worktree_remove
   ├─ write.rs      shared guards for write tools (refusals, names, identity)
   ├─ branches.rs   create_branch, delete_branch, rename_branch, create_tag
   ├─ staging.rs    stage, unstage
   ├─ commit.rs     commit, reset
   ├─ checkout.rs   restore, switch
   ├─ clean.rs      clean
   ├─ stash.rs      stash_push, stash_pop, stash_drop
   ├─ integrate.rs  merge, cherry_pick, revert, continue_operation, abort_operation
   ├─ rebase.rs     rebase (a sequencer using git's .git/rebase-merge layout)
   ├─ submodules.rs list_submodules
   ├─ config.rs     config_get (with secret redaction)
   ├─ objects.rs    count_objects
   ├─ prompts.rs    write_commit_message, review_changes, explain_history prompts
   └─ history.rs    log, show_commit, file_history, search_commits
scripts/smoke.sh      stdio MCP smoke test
scripts/demo-repo.sh  builds the demo workspace the docs examples come from
docs/                 user and contributor documentation
```
