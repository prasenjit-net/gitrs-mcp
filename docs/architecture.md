# Architecture

gitrs-mcp is a single binary that serves one Git repository to an MCP client over stdin/stdout. It reads the repository with [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`), a pure-Rust Git implementation, and speaks MCP through the official Rust SDK, [`rmcp`](https://crates.io/crates/rmcp). It never runs the `git` executable, and it has no network code.

## Layers

```
src/
├─ main.rs          CLI arguments, logging to stderr, stdio transport
├─ server/          MCP surface: server metadata (mod.rs) and one #[tool_router] per area
│  ├─ repo.rs       repository, refs and revisions
│  ├─ changes.rs    working tree and index
│  ├─ files.rs      file contents and blame
│  └─ history.rs    commit history
├─ repo.rs          RepoHandle: opens the repository, runs gix calls on the blocking pool
└─ tools/           Git logic, as plain synchronous functions over &gix::Repository
   ├─ mod.rs        shared types and helpers (Signature, LogEntry, ChangeKind, pagination, paths), test fixtures
   ├─ repo.rs       repo_info
   ├─ refs.rs       list_refs
   ├─ revision.rs   rev_parse, compare_refs
   ├─ status.rs     status
   ├─ diff.rs       diff, plus the tree/index comparison that status reuses
   ├─ files.rs      read_file, list_tree
   ├─ blame.rs      blame
   └─ history.rs    log, show_commit, file_history, search_commits
```

Each layer has one job, and dependencies only point downward:

| Layer | Owns | Must not |
|---|---|---|
| `main.rs` | Argument parsing, logging setup, starting the stdio transport | Contain tool logic |
| `server/` | Registering tools with rmcp, their titles and annotations, server instructions | Touch gix internals |
| `repo.rs` | Opening the repository and bridging sync gix calls into async rmcp handlers | Know about individual tools |
| `tools/` | All Git behaviour, plus the argument and result types that become JSON Schemas | Import `rmcp` or `server` |

Because `tools/` doesn't depend on MCP at all, every tool is an ordinary function, `fn(&gix::Repository, Args) -> anyhow::Result<Out>`, and its unit tests call it directly.

## Data flow of a tool call

1. The client sends `tools/call` with a tool name and JSON arguments.
2. rmcp deserializes the arguments into the tool's `Args` struct. A malformed request (wrong type, missing required field) is rejected here, before any Git code runs.
3. The `#[tool]` wrapper in `server/<area>.rs` passes a closure to `RepoHandle::run`.
4. `RepoHandle::run` moves the closure to tokio's blocking thread pool, gets a thread-local `gix::Repository`, and calls the tool function.
5. The tool returns `Ok(Out)` or an `anyhow::Error`. A success becomes `structuredContent` (and a text copy) through rmcp's `Json<T>` wrapper. An error is formatted with its full context chain and returned as a tool error (`isError: true`), so the model can read it and retry. Protocol-level errors are reserved for server bugs.

```
client ──tools/call──▶ rmcp ──Args──▶ server wrapper ──closure──▶ RepoHandle::run
                                                                        │ spawn_blocking
                                                                        ▼
client ◀──result───── rmcp ◀──Json<Out> / Err(String)────── tools::<area>::<tool>(&repo, args)
```

## The sync/async bridge

gix is synchronous and `gix::Repository` is not `Sync`, while rmcp handlers are async. `RepoHandle` holds a `gix::ThreadSafeRepository`, which is cheap to clone and can be shared between threads. For each call it clones the handle into a `spawn_blocking` task and turns it into a thread-local `Repository` there. Long Git operations therefore never block the async runtime, and concurrent calls each get their own repository instance with its own object caches.

## Repository discovery and paths

At startup `RepoHandle::discover` finds the repository containing `--repo` (searching parent directories like `git` does), then makes its root the process working directory and reopens it from there. gix resolves pathspecs against the working directory captured when a repository is opened. Starting from the root guarantees that every path and pathspec argument is relative to the repository root, even when the server was launched from a subdirectory.

Path arguments go through one normalizer (`tools::repo_path`). It strips `./` and trailing slashes, and rejects absolute paths and `..`, so a path can't name anything outside the repository.

## Shared conventions in code

`tools/mod.rs` holds the helpers every tool uses, so behaviour stays the same everywhere:

- `resolve_commit` / `resolve_tree`: revision defaults (`HEAD`), tag peeling, and actionable errors. An unborn HEAD gets its own message ("has no commits yet").
- `page_limit`, `paginate`: limits clamped to a per-tool default and maximum; pagination that reports `truncated` by pulling at most one item past the page, so lazy history walks stay cheap.
- `byte_budget`: the per-call content budget (64 KiB default, 1 MiB maximum).
- `Signature`, `LogEntry`, `ChangeKind`: the output types several tools share.

## Matching git

Results are meant to match the `git` CLI on the same repository, and the tests check this by running real `git` as an oracle. A few places needed care beyond calling gix:

- **Diff text.** `diff` renders patches itself on top of gix's line diff, so that they match `git diff` byte for byte: lines are compared with their terminators, so a missing final newline gets `\ No newline at end of file`; hunk ranges are printed in git's compact form (`-1 +1,2`); and hunk headers carry git's default function context (the nearest preceding line that starts with a letter, `_` or `$`). Custom diff drivers from `.gitattributes` aren't applied to the function context.
- **History simplification.** `file_history` and path-filtered `search_commits` use their own walk, which implements git's default rule for `git log -- <path>`. A merge whose content at the path equals one parent's is hidden, and only that parent is followed.
- **gix 0.90 quirks worked around.** When copy detection is enabled, gix reports a copy of a file that was itself modified as a copy of the *new* content, and drops that file's own modification; `diff` and `status` repair both. gix also looks up `status.renames` under the wrong section, so the tree-to-index comparison reads `status.renames` and `diff.renames` itself.

## Safety model

- **Local only, by construction.** gix is compiled without any network transport feature, so fetch, push, clone and `ls-remote` can't be reached from any code path. `status.upstream` reports the configured upstream from local config only; nothing is ever fetched.
- **Read-only.** Every current tool is read-only and annotated with `readOnlyHint`. The `--allow-write` flag is reserved for the write tools planned in Phase 3 of the [roadmap](../ROADMAP.md). Those tools will only be registered when the flag is given, so without it the model never sees them.
- **Filesystem reads stay inside the repository.** Reads from the working tree (`read_file` with `rev: "worktree"`) refuse anything under `.git/` and any path whose parent directory resolves outside the repository through a symlink. A symlink itself is reported by its target, as git stores it, and never followed.
- **Bounded output.** Every list is paginated and every content field has a byte budget, so a single call can't flood the client's context.
- **Errors for the model, not crashes.** Tool code returns errors instead of panicking (`unwrap` and `expect` are only allowed in tests), and every error message says what to change.
