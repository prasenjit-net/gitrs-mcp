# Architecture

gitrs-mcp is a single binary that serves one Git repository to an MCP client over stdin/stdout. It reads the repository with [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix`), a pure-Rust Git implementation, and speaks MCP through the official Rust SDK, [`rmcp`](https://crates.io/crates/rmcp). It never runs the `git` executable, and it has no network code.

## Layers

```
src/
├─ main.rs          CLI arguments, logging to stderr, stdio transport
├─ setup/           `gitrs-mcp setup`: registering the server with agents (below)
├─ lib.rs           the library half: private modules, GitServer for main.rs, parsing entry points for fuzz/
├─ server/          MCP surface: server metadata (mod.rs) and one #[tool_router] per area
│  ├─ repo.rs       repository, refs, tags, reflogs and revisions
│  ├─ changes.rs    working tree, index, conflicts and merge previews
│  ├─ files.rs      file contents, search, ignore and attribute rules, blame
│  ├─ history.rs    commit history and notes
│  ├─ setup.rs      worktrees, submodules, config and object storage
│  ├─ resources.rs  resources and completion, mapped onto rmcp's types
│  ├─ prompts.rs    the #[prompt_router]: one wrapper per prompt
│  ├─ progress.rs   run_watched: request cancellation and progress tokens for long tools
│  ├─ watch.rs      --watch: resource subscriptions and the polling task that notifies them
│  ├─ repos.rs      the served repositories, the InRepo/RepoOnly argument wrappers, client roots
│  └─ write.rs      write tools; only registered with --allow-write
├─ repo.rs          RepoHandle: finds and opens a repository, runs gix calls on the blocking pool
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
   ├─ progress.rs   Watch: cancellation flag and progress reports, without MCP types
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

## The setup layer

`src/setup/` is separate from everything above: it imports neither `rmcp` nor `tools` nor `server`, and never opens a repository. It is a small pipeline:

```
SetupArgs ─► pick agents ─► plan (read only) ─► show ─► confirm ─► apply
```

| File | Role |
|---|---|
| `mod.rs` | The arguments, the pipeline above, the report. No agent specifics. |
| `agents.rs` | The registry: one `Agent` per client with how to detect it, its config files per scope, its format, and when the format was last verified. |
| `plan.rs` | Works out what would happen for one agent and scope (`Verdict`) and what applying it changes (`Effect`). Reads files and, for Claude Code, runs `claude mcp get`; writes nothing. |
| `document.rs` | The `Document` trait: get, set, remove one entry in a parsed config file. |
| `json.rs`, `toml.rs` | `Document` for JSON (`serde_json` with ordered keys) and TOML (`toml_edit`, which keeps comments). |
| `files.rs` | Reading, backing up and atomically replacing a file. |
| `cli.rs` | Running `claude` with a fixed argument list and a timeout. |
| `binary.rs` | Which command to register (absolute path or bare name, and the Homebrew rule). |
| `env.rs` | Everything read from the machine (home, `PATH`, working directory, this executable), so tests can use a temporary one. |
| `prompt.rs` | The checklist and yes/no questions, over injected input and output. |

Planning and applying are separate so `--dry-run`, `--status` and the confirmation can show the plan without side effects. Because the file formats share the `Document` trait, the add, replace, remove and compare rules are written once.

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

### Progress and cancellation

Long walks get a `tools::progress::Watch`, so the tool layer can report progress and notice cancellation without knowing about MCP. For `log`, `search_commits`, `file_history`, `grep` and `blame`, the wrapper calls `GitServer::run_watched` (in `server/progress.rs`) instead of `RepoHandle::run`. That builds the watch from the request:

- A small task waits on the request's cancellation token and sets the watch's `AtomicBool` when the client sends `notifications/cancelled`. Tools wrap their commit or file iterator in `Watch::track`, which checks the flag on every item and ends the walk with an error. rmcp drops the response of a cancelled request, so the error only stops the work.
- If the request has a progress token, reports go through a channel to a task that sends `notifications/progress`, at most one every 250 ms. `run_watched` waits for that task to drain before returning, so every notification reaches the client before the result.

Other callers, such as the prompts, pass `Watch::none()`.

### Resource subscriptions

With `--watch`, `GitServer` owns a `server::watch::Watcher`, which holds the subscribed URIs and the fingerprint last seen for each (`tools::resources::fingerprint`). A polling task starts with the first subscription. It runs once per interval on the blocking pool, and holds only a weak reference to the watcher, so it ends with the server. URIs whose fingerprint changed are sent to legacy subscribers through the client's peer, and broadcast to open `subscriptions/listen` streams, each of which forwards the URIs it accepted. For `subscriptions/listen`, rmcp acknowledges before calling `listen`, so the baselines are taken in `accepted_subscription_filter`: a change right after the acknowledgment is still reported.

## The sync/async bridge

gix is synchronous and `gix::Repository` is not `Sync`, while rmcp handlers are async. `RepoHandle` holds a `gix::ThreadSafeRepository`, which is cheap to clone and can be shared between threads. For each call it clones the handle into a `spawn_blocking` task and uses a thread-local `Repository` there. Long Git operations therefore never block the async runtime, and concurrent calls each get their own repository instance.

Each blocking-pool thread keeps its thread-local repositories between calls, one per handle, with a 16 MiB object cache (`object_cache_size_if_unset`), so repeated calls find objects already decoded. Objects are immutable, so the cache can't go stale. Refs, the index and packs are still read from disk, so changes made by git between calls are seen. A repository is taken out of the thread's map while a call uses it, so a panic only drops that instance. gix uses the commit-graph file, when the repository has one, on every revision walk.

## Repository discovery and paths

At startup `RepoHandle::find_root` finds the repository containing each `--repo`, searching parent directories like `git` does. Then `RepoHandle::leave_worktrees` makes the filesystem root the process working directory, and only then are the repositories opened.

This matters because gix resolves pathspecs against the working directory captured when a repository is opened: relative to the working tree, when that directory is inside it. From outside every working tree, each repository resolves paths from its own root. Every path and pathspec argument is therefore relative to the chosen repository's root, even when the server was launched from a subdirectory, and with several repositories open at once.

### Several repositories

`server::repos::Repos` holds the served repositories: the `--repo` ones, the first being the default, and those at the client's MCP roots. Every tool wrapper takes `Parameters<InRepo<Args>>`, where `InRepo` adds an optional `repo` field and `#[serde(flatten)]`s the tool's own arguments, so the published schema is the tool's plus one property. `GitServer::repo(name)` picks the handle: a name, or a path inside a served repository (resolved through symlinks, and refused if relative or containing `..`). Tool functions in `tools/` stay single-repository.

Resources take `?repo=<name>`, which `server::repos::split_repo` removes before `tools::resources` sees the URI. Prompts take a `repo` argument, and completion suggests repository names for it.

Client roots are listed after `notifications/initialized`, and again on `notifications/roots/list_changed`, on a separate task, since notification handlers run inside rmcp's message loop. A root is served only if the repository found there has its root inside the declared directory.

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
- **Untrusted repositories are read-only.** A repository the current user doesn't own, and that isn't listed in `safe.directory`, opens with reduced trust, so gix ignores its repository-local config. Every write wrapper selects its repository through `GitServer::writable`, which refuses such repositories.
- **Filesystem reads stay inside the repository.** Reads from the working tree (`read_file` with `rev: "worktree"`, and `grep`, which skips such paths) refuse any path with a `.git` component, whatever its case and with trailing dots or spaces, and any path whose parent directory resolves outside the repository through a symlink. A symlink itself is reported by its target, as git stores it, and never followed.
- **Secrets stay out of output.** `config_get` and `list_submodules` replace values that may be secrets with `<redacted>`: anything under `credential`, keys named like a password, token or secret, `http.extraHeader`, and the password in any URL. Configuration that gix derives from environment variables isn't listed at all, as in `git config --list`.
- **Bounded output.** Every list is paginated and every content field has a byte budget, so a single call can't flood the client's context.
- **Errors for the model, not crashes.** Tool code returns errors instead of panicking (`unwrap` and `expect` are only allowed in tests), and every error message says what to change.

## Known gix gaps and workarounds

gitrs-mcp is built on gix 0.90. These are the places where gix is missing something or behaves differently from git, found while checking each tool against the git CLI, and what the code does about each. Re-check them when upgrading gix: a gap that is closed upstream lets the workaround be deleted.

| Missing in gix | Impact | Workaround |
|---|---|---|
| Stash | `stash_*` tools | Implement the stash commit format ourselves (Phase 4). Reading `refs/stash` works today |
| Sequencer (rebase, cherry-pick, revert state machine) | Phase 4 | Own minimal sequencer on `merge_trees`, writing git-compatible state files |
| Hooks | `commit` skips `pre-commit`/`commit-msg` | Document it. Optional `--run-hooks` executes them as subprocesses |
| Commit/tag signing | Signed commits | Out of scope at first. Maybe shell out to `gpg`/`ssh-keygen -Y sign` later |
| Network | n/a | Intentionally excluded |
| Copy detection drops a modified copy source (found in Phase 1) | With `diff.renames=copies`, a copy whose source was also modified is reported as a copy of the source's *new* content, and the source's own modification disappears | Worked around in `tools/diff.rs` (`repair_copies`): point the copy back at the old content and restore the source's change |
| `status.renames` read from the wrong section (found in Phase 1) | gix looks the key up as `merge.renames`, so `status.renames` is ignored | Worked around in `tools/diff.rs` (`index_rename_tracking`): resolve `status.renames`, then `diff.renames`, ourselves |
| Ignore matching only looks at the path itself (found in Phase 2) | `excludes()` → `at_entry()` reports no rule for `target/foo.rs` when `target/` is ignored; git ignores everything below an ignored directory | `tools/attributes.rs` (`deciding_rule`) checks each parent directory first, as git does |
| Environment variables appear as configuration (found in Phase 2) | The config snapshot contains values gix derives from `GIT_EDITOR`, `HTTPS_PROXY` and others (some under `gitoxide.*` keys), which `git config --list` doesn't show | `config_get` leaves out `Source::EnvOverride` sections |
| `key =` and a bare `key` look the same when iterating config (found in Phase 2) | The section iterator yields an empty value for both, but git reads the first as the empty string and the second as `true` | `config_get` asks `value_implicit` for empty values |
| Index writes keep a stale cache-tree (found in Phase 3) | `gix_index::File::write` writes the `TREE` extension unchanged after entries change, so a later `git commit` could record outdated subtrees ([gitoxide#2421](https://github.com/GitoxideLabs/gitoxide/issues/2421)) | `update_index` writes only the end-of-index-entry extension; git rebuilds the rest |
| A bare `.` pathspec is matched literally (found in Phase 6) | gix leaves `.` and `./` unnormalized, so with case-sensitive matching (Linux) they match nothing: `restore` and `reset` with `paths: ["."]` failed. macOS hid it, since `core.ignoreCase` routes matching elsewhere | `pathspec_patterns` passes whole-tree patterns as `:/` |
| Blame doesn't know about shallow clones (found in Phase 6) | `blame_file` follows a boundary commit's recorded parents, which aren't there, and fails; git attributes the remaining lines to the boundary commit | `blame` explains the cause and suggests `git fetch --unshallow`. History walks and `parents` use `visible_parents`, which treats boundary commits as roots, as git does |
| Unified diff output differs from git (found in Phase 1) | gix's unified-diff helpers print `,1` ranges and no function context, and its default line input strips terminators, which loses `\ No newline at end of file` | Own hunk writer in `tools/diff.rs` on top of gix's line diff |
