# Architecture

gitrs-mcp is a single binary that serves one Git repository to an MCP client over stdin/stdout. It reads the repository with [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`), a pure-Rust Git implementation, and speaks MCP through the official Rust SDK, [`rmcp`](https://crates.io/crates/rmcp). It never runs the `git` executable, and it has no network code.

## Layers

```
src/
├─ main.rs          CLI arguments, logging to stderr, stdio transport
├─ server/          MCP surface: server metadata (mod.rs) and one #[tool_router] per area
│  ├─ repo.rs       repository, refs, tags, reflogs and revisions
│  ├─ changes.rs    working tree, index, conflicts and merge previews
│  ├─ files.rs      file contents, search, ignore and attribute rules, blame
│  ├─ history.rs    commit history and notes
│  ├─ setup.rs      worktrees, submodules, config and object storage
│  ├─ resources.rs  resources and completion, mapped onto rmcp's types
│  ├─ prompts.rs    the #[prompt_router]: one wrapper per prompt
│  └─ write.rs      write tools; only registered with --allow-write
├─ repo.rs          RepoHandle: opens the repository, runs gix calls on the blocking pool
└─ tools/           Git logic, as plain synchronous functions over &gix::Repository
   ├─ mod.rs        shared types and helpers (Signature, LogEntry, ChangeKind, pagination, paths), test fixtures
   ├─ repo.rs       repo_info
   ├─ resources.rs  gitrs:// URIs: parsing, reading (built on show_commit, diff and read_file), completion
   ├─ refs.rs       list_refs, describe, show_tag
   ├─ reflog.rs     reflog, list_stashes (git keeps stashes as the reflog of refs/stash)
   ├─ notes.rs      notes
   ├─ revision.rs   rev_parse, compare_refs
   ├─ status.rs     status
   ├─ diff.rs       diff, plus the tree/index comparison that status reuses
   ├─ merge.rs      conflicts, merge_preview (an in-memory merge)
   ├─ files.rs      read_file, list_tree
   ├─ grep.rs       grep
   ├─ attributes.rs check_ignore, check_attr
   ├─ blame.rs      blame
   ├─ worktrees.rs  list_worktrees, worktree_add, worktree_remove
   ├─ write.rs      guards shared by the write tools: refusals, name validation, identity, message cleanup
   ├─ branches.rs   create_branch, delete_branch, rename_branch, create_tag
   ├─ staging.rs    stage, unstage
   ├─ commit.rs     commit, reset
   ├─ checkout.rs   restore, switch: working-tree writes
   ├─ clean.rs      clean
   ├─ stash.rs      stash_push, stash_pop, stash_drop (git's stash layout, built on gix primitives)
   ├─ integrate.rs  merge, cherry_pick, revert, continue_operation, abort_operation (three-way merges and git's state files)
   ├─ rebase.rs     rebase, a sequencer that keeps its plan in git's .git/rebase-merge layout
   ├─ submodules.rs list_submodules
   ├─ config.rs     config_get, plus the secret redaction submodule URLs share
   ├─ objects.rs    count_objects
   ├─ prompts.rs    prompt text: diffs, blame and history filled into instructions
   └─ history.rs    log, show_commit, file_history, search_commits
```

Each layer has one job, and dependencies only point downward:

| Layer | Owns | Must not |
|---|---|---|
| `main.rs` | Argument parsing, logging setup, starting the stdio transport | Contain tool logic |
| `server/` | Registering tools with rmcp, their titles and annotations, server instructions; mapping resources, prompts and completion onto rmcp's types | Touch gix internals |
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

- `resolve_commit` / `resolve_tree` / `resolve_object`: revision defaults (`HEAD`), tag peeling (none for `resolve_object`), and actionable errors. An unborn HEAD gets its own message ("has no commits yet").
- `page_limit`, `paginate`: limits clamped to a per-tool default and maximum; pagination that reports `truncated` by pulling at most one item past the page, so lazy history walks stay cheap.
- `byte_budget`, `clip_utf8`: the per-call content budget (64 KiB default, 1 MiB maximum), and cutting text to it without splitting a character.
- `Signature`, `LogEntry`, `ChangeKind`, `ObjectType`: the output types several tools share.

## Matching git

Results are meant to match the `git` CLI on the same repository, and the tests check this by running real `git` as an oracle. A few places needed care beyond calling gix:

- **Diff text.** `diff` renders patches itself on top of gix's line diff, so that they match `git diff` byte for byte: lines are compared with their terminators, so a missing final newline gets `\ No newline at end of file`; hunk ranges are printed in git's compact form (`-1 +1,2`); and hunk headers carry git's default function context (the nearest preceding line that starts with a letter, `_` or `$`). Custom diff drivers from `.gitattributes` aren't applied to the function context.
- **History simplification.** `file_history` and path-filtered `search_commits` use their own walk, which implements git's default rule for `git log -- <path>`. A merge whose content at the path equals one parent's is hidden, and only that parent is followed.
- **gix 0.90 quirks worked around.** When copy detection is enabled, gix reports a copy of a file that was itself modified as a copy of the *new* content, and drops that file's own modification; `diff` and `status` repair both. gix also looks up `status.renames` under the wrong section, so the tree-to-index comparison reads `status.renames` and `diff.renames` itself.

## Safety model

- **Local only, by construction.** gix is compiled without any network transport feature, so fetch, push, clone and `ls-remote` can't be reached from any code path. `status.upstream` reports the configured upstream from local config only; nothing is ever fetched.
- **Read-only by default.** Without `--allow-write`, every tool is read-only and annotated with `readOnlyHint`. `merge_preview` computes a full merge, but the blobs and trees it creates go to an in-memory object store that is dropped afterwards; a test checks that `.git/objects` is unchanged after a preview. As with `git merge-tree`, merge drivers and filters from the user's git configuration can run during the merge.
- **Writes are opt-in and guarded.** The write tools live in their own router (`server/write.rs`), which `GitServer::new` adds only when `--allow-write` is given, so without the flag the model never sees them. Each write tool:
  - runs every check before writing anything, so a refusal leaves the repository byte for byte unchanged (each tool has a test that snapshots the whole repository around a refused call);
  - refuses to move HEAD, rewrite the index or touch the working tree while a merge, rebase, cherry-pick, revert or bisect is in progress (`tools::write::ensure_idle`). The exceptions are `stage`, which resolves conflicts, and `continue_operation` / `abort_operation`, which finish or undo the operation;
  - computes a merge, cherry-pick, revert or rebase step in an in-memory object store first and checks it against the index and working tree, then repeats it for real under the index lock. When it stops on conflicts, it leaves the state git leaves (conflict stages, markers, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `MERGE_MSG`, `ORIG_HEAD`, `.git/rebase-merge/`), so `git commit`, `git rebase --continue` or `git merge --abort` can take over, and the reverse. Tests interleave gitrs-mcp and git steps to check this;
  - updates refs as compare-and-swap edits (`PreviousValue::MustExistAndMatch` / `MustNotExist`) under git's lock files, with git's reflog messages;
  - defaults to `dry_run: true` when it can lose data, and is annotated with `destructiveHint` then;
  - needs a committer identity from git config and refuses without one, rather than inventing it, and refuses to create unsigned commits or annotated tags when `commit.gpgSign` / `tag.gpgSign` asks for signatures;
  - edits the index through `tools::write::update_index`, which takes `.git/index.lock` before reading (so a concurrent `git add` isn't lost), applies the edit, and writes the index back without the cache-tree, untracked-cache and fsmonitor extensions, which would be stale and which git rebuilds. A split index is merged with its shared index when read and written back whole; a sparse index is refused. When `stage` hashes files it runs the same clean filters and line-ending conversion as `git add`;
  - writes working-tree files (`restore`, `switch`) through gix's checkout, with smudge filters, line endings, symlinks and the executable bit as `git checkout` writes them, after hashing the current files in an in-memory object store to check that nothing local would be overwritten. `switch` applies git's two-way rule: a path that differs between the two commits must be clean in the index and working tree, and no untracked file may be in the way. Paths whose nearest existing directory leads outside the working tree through a symlink are never written.
- **Filesystem reads stay inside the repository.** Reads from the working tree (`read_file` with `rev: "worktree"`, and `grep`, which skips such paths) refuse anything under `.git/` and any path whose parent directory resolves outside the repository through a symlink. A symlink itself is reported by its target, as git stores it, and never followed.
- **Secrets stay out of output.** `config_get` and `list_submodules` replace values that may be secrets with `<redacted>`: anything under `credential`, keys named like a password, token or secret, `http.extraHeader`, and the password in any URL. Configuration that gix derives from environment variables isn't listed at all, as in `git config --list`.
- **Bounded output.** Every list is paginated and every content field has a byte budget, so a single call can't flood the client's context.
- **Errors for the model, not crashes.** Tool code returns errors instead of panicking (`unwrap` and `expect` are only allowed in tests), and every error message says what to change.
