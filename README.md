# gitrs-mcp

A local-only Git [MCP](https://modelcontextprotocol.io) server written in Rust, built on [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`) and the official Rust MCP SDK ([`rmcp`](https://crates.io/crates/rmcp)).

It lets AI assistants inspect a Git repository through typed tools with structured JSON output. It doesn't shell out to `git` and doesn't parse porcelain text.

- **Local only, by construction.** gix is compiled without any network features, so fetch, push and clone can't happen.
- **Read-only by default.** Write tools (coming in Phase 3) will exist only when you start the server with `--allow-write`.
- **Git-compatible.** Results match the `git` CLI on the same repository: the test suite checks each tool against real `git` output, and generated patches apply with `git apply`.
- **Structured output.** Every tool returns JSON with a published output schema. Bad input, such as an unknown revision, comes back as a tool error the model can read and correct.
- **Pure Rust.** It runs as a single static binary that speaks MCP over stdio, and it doesn't need `libgit2` or the `git` executable.

> **Status:** early development. Phases 0–2 of the [roadmap](ROADMAP.md) are done: 27 read-only tools covering status, diffs, merge previews and conflicts, file contents, code search, ignore and attribute rules, history, blame, refs, tags, reflogs, stashes, notes, worktrees, submodules, config and object stats. Write tools come in Phase 3.

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

Setup for other clients, logging, and the git settings that affect results are in [docs/configuration.md](docs/configuration.md).

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
| `repo_info` | none | git dir, worktree, bare/shallow flags, current branch, HEAD commit, in-progress operation (merge, rebase, …) |
| `list_worktrees` | `limit?`, `skip?` | Main and linked worktrees: path, HEAD, branch, locked/prunable, which one is served |
| `list_submodules` | `limit?`, `skip?` | Submodules with URL (passwords redacted), recorded and checked-out commits, and state, like `git submodule status` |
| `config_get` | `key?` or `section?`, `scope?`, `limit?`, `skip?` | Config values with scope and file; credentials, tokens, passwords and auth headers redacted |
| `count_objects` | none | Loose and packed object counts and sizes, packs, commit-graph and multi-pack-index presence |

The full reference, with every argument, result field, error and a real example per tool, is in [docs/tools.md](docs/tools.md). Phase 3 adds safe write tools behind `--allow-write`; see [ROADMAP.md](ROADMAP.md).

## Documentation

- [docs/tools.md](docs/tools.md): tool reference.
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
```

Every target accepts `REPO=<path>`, `ARGS="<extra server flags>"` and `LOG=<level>`. `make smoke` also takes `FILE=<tracked file>` (default `README.md`) for the tools that need a path.

Contributor rules (code quality, MCP documentation, testing and the pre-commit checklist) are in [CLAUDE.md](CLAUDE.md), and the step-by-step guide to adding a tool is in [docs/development.md](docs/development.md).

### Layout

```
src/
├─ main.rs          CLI arguments, logging, stdio transport
├─ server/          MCP tool registration: thin async wrappers, one router per area
├─ repo.rs          Thread-safe repo handle; runs gix calls on the blocking pool
└─ tools/           Git logic as plain sync functions over &gix::Repository
   ├─ mod.rs        shared types and helpers, test fixtures
   ├─ repo.rs       repo_info
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
   ├─ worktrees.rs  list_worktrees
   ├─ submodules.rs list_submodules
   ├─ config.rs     config_get (with secret redaction)
   ├─ objects.rs    count_objects
   └─ history.rs    log, show_commit, file_history, search_commits
scripts/smoke.sh      stdio MCP smoke test
scripts/demo-repo.sh  builds the demo workspace the docs examples come from
docs/                 user and contributor documentation
```
