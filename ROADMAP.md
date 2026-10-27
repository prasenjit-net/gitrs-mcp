# gitrs-mcp roadmap

A local-only Git MCP server in Rust on top of [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix` 0.90) and the official MCP SDK (`rmcp` 3.7), speaking MCP over stdio.

## Status

_Last updated 2026-10-27._

| Phase | Status |
|---|---|
| 0 Scaffold | ✅ Done: 4 tools |
| 1 Core read | ✅ Done: 9 tools, each checked against the git CLI |
| 2 Deep read and inspection | ✅ Done: 14 tools, each checked against the git CLI |
| 3 Safe writes | ⏭️ Next |
| 4 History rewriting | Planned |
| 5 MCP-native extras | Planned |
| 6 Hardening and release | Planned |

The server has 27 read-only tools. 136 unit tests and the end-to-end smoke test pass, with 95.1% line coverage. `--allow-write` is accepted but registers nothing until Phase 3.

## Design principles

1. **Local only, by construction.** `gix` is built without any `*-network-client` / `*-http-transport-*` feature, so fetch, push, clone and ls-remote can't be compiled in. Tools never read remote credentials.
2. **Read-only by default.** Write tools exist only behind `--allow-write`. Without the flag they aren't registered at all, so the model never sees them (not just "refused at call time").
3. **Structured, bounded output.** Every tool returns typed JSON (`Json<T>` → `structuredContent` plus `outputSchema`). Every list is paginated (`limit`, `skip`/`cursor`), and every blob or patch has a byte budget with an explicit `truncated: true`. The default output budget is about 64 KiB per call.
4. **Errors the model can act on.** Bad input (an unknown revision, a path not in the tree, a dirty worktree) comes back as a tool error (`isError: true`) whose message says what to fix. Protocol errors are reserved for real server bugs.
5. **Git-compatible semantics.** Revisions accept full `git rev-parse` syntax. Results must match the `git` CLI on the same repo. Tests use system `git` as the oracle.
6. **Destructive operations need two steps.** Anything that can lose data takes `dry_run` (default `true`) and reports exactly what it would change. The real run needs `dry_run: false`.

## Architecture

```
src/
├─ main.rs          CLI args (clap), logging to stderr, stdio transport
├─ server/          ServerHandler metadata (mod.rs) + one #[tool_router] of thin async wrappers per area
├─ repo.rs          RepoHandle: ThreadSafeRepository + spawn_blocking bridge
└─ tools/
   ├─ mod.rs        shared output types (Signature, LogEntry, ChangeKind), arg helpers, test fixtures
   ├─ repo.rs       repo_info
   ├─ refs.rs       list_refs, describe, show_tag
   ├─ reflog.rs     reflog, list_stashes
   ├─ notes.rs      notes
   ├─ revision.rs   rev_parse, compare_refs
   ├─ status.rs     status
   ├─ diff.rs       diff (and the tree↔index comparison status reuses)
   ├─ merge.rs      conflicts, merge_preview
   ├─ files.rs      read_file, list_tree
   ├─ grep.rs       grep
   ├─ attributes.rs check_ignore, check_attr
   ├─ blame.rs      blame
   ├─ worktrees.rs  list_worktrees
   ├─ submodules.rs list_submodules
   ├─ config.rs     config_get
   ├─ objects.rs    count_objects
   └─ history.rs    log, show_commit, file_history, search_commits
```

Conventions for every new tool:
- **Logic.** A sync `fn(&gix::Repository, Args) -> anyhow::Result<Out>` in `tools/<area>.rs`, with `Args: Deserialize + JsonSchema` and `Out: Serialize + JsonSchema`. Doc comments on the fields become the schema descriptions the model reads.
- **Wrapper.** A three-line wrapper in the area's `server/<area>.rs` router, annotated with `read_only_hint`, `destructive_hint` and `idempotent_hint` as applicable.
- **Tests.** Unit tests in the same file against `testutil::fixture_repo()`. When output must match git, compare against the `git` CLI.

`server.rs` was split at the start of Phase 2 into `server/<area>.rs` files, each with one `#[tool_router(router = <area>_router)]` block; `GitServer::new` combines them with `+`.

---

## Phase 0: Scaffold ✅ (done)

| Tool | Purpose | gix API |
|---|---|---|
| `repo_info` | Paths, HEAD, current branch, bare/shallow, in-progress op | `head_name`, `head_id`, `state`, `is_shallow` |
| `log` | Paginated history from a revision; `first_parent` option | `rev_walk().first_parent_only()` |
| `show_commit` | Full metadata and message of one commit | `rev_parse_single`, `peel_to_commit` |
| `list_refs` | Branches, tags or all refs, with peeled targets and a HEAD marker | `references().local_branches()/tags()/all()` |

Infrastructure is in place: `--repo`, `--allow-write`, tracing to stderr via `GITRS_LOG`, tool-level error mapping, the fixture repo built with system git, and a verified stdio round trip.

---

## Phase 1: Core read (the everyday 80%) ✅ (done)

These tools let an agent understand "what's going on in this repo" without shelling out.

All nine tools shipped, each tested against the git CLI (`git status --porcelain=v2`, `git diff` numstat, name-status and full patch text, `git apply --index`, `git ls-tree`, `git log -- <path>`, `git log --follow`, `git rev-list`, `git rev-parse`, `git merge-base`, `git blame --porcelain`). The full reference is in [docs/tools.md](docs/tools.md). Where the implementation differs from the specs below, the reasons are in [Phase 1: deviations from the plan](#phase-1-deviations-from-the-plan).

### `status` ✅
- **In:** `pathspec?: string[]`, `include_untracked: bool = true`, `include_ignored: bool = false`.
- **Out:** `branch`, `head`, `upstream: null` (local-only), `staged[]`, `unstaged[]`, `untracked[]`, `conflicted[]`. Each entry has `{path, change: added|modified|deleted|renamed|typechange, old_path?}`.
- **gix:** `repo.status(progress)` then `.into_iter()` (index↔worktree and tree↔index). Untracked files come from the `dirwalk` integration.
- **Tests:** compare with `git status --porcelain=v2` for every change type, including renames and a submodule.

### `diff` ✅
- **In:** `from?: rev`, `to?: rev`, `staged: bool`, `pathspec?: string[]`, `format: stat|name_status|patch = stat`, `context_lines = 3`, `max_bytes`.
- **Modes:** worktree↔index (default), index↔HEAD (`staged`), and rev↔rev.
- **Out:** `files[] {path, old_path?, status, additions, deletions, binary, patch?}`, `totals`, `truncated`.
- **gix:** `diff_tree_to_tree` with rename tracking, the `status` iterator for the worktree/index sides, `diff_resource_cache` with `gix-imara-diff` for unified hunks.
- **Tests:** match `git diff --numstat` and `--name-status`. Patch text must apply cleanly with `git apply --check`.

### `read_file` ✅
- **In:** `path`, `rev = HEAD` (or `"index"` / `"worktree"`), `offset_bytes?`, `max_bytes`.
- **Out:** `content` (utf-8) or `base64` with `binary: true`, plus `size`, `blob_id`, `truncated`.
- **gix:** `rev_parse_single("<rev>:<path>")`, then `object().detach()`. Index reads use `index().entry_by_path`.

### `list_tree` ✅
- **In:** `rev = HEAD`, `path?`, `recursive = false`, `limit`.
- **Out:** `entries[] {path, kind: blob|tree|commit(submodule)|symlink, mode, id, size?}`.
- **gix:** `commit.tree()`, then `peel_to_entry_by_path` and `traverse().breadthfirst`.

### `rev_parse` ✅
- **In:** `spec` (also ranges such as `a..b` and `a...b`).
- **Out:** `{kind: single|range|merge_base_range, ids[], object_type}`.
- **gix:** `repo.rev_parse(spec)`, which returns a `revision::Spec`.

### `file_history` ✅
- **In:** `path`, `rev = HEAD`, `limit`, `follow_renames = false`.
- **Out:** a list of `LogEntry` plus `change` for the path at each commit.
- **gix:** walk and tree-diff each commit against its first parent, filtered by path. Rename following comes later, behind a flag.

### `search_commits` ✅
- **In:** `message_regex?`, `author?`, `since?`, `until?`, `path?`, `rev = HEAD`, `limit`.
- **gix:** `rev_walk` with the commit-time cutoff `Sorting::ByCommitTimeCutoff` and in-process filtering (`regex` is already pulled in by `revparse-regex`).

### `compare_refs` ✅
- **In:** `base`, `head`.
- **Out:** `merge_base`, `ahead`, `behind`, `ahead_commits[]` (capped), `behind_commits[]` (capped).
- **gix:** `merge_base`, plus a `rev_walk(...).with_hidden(...)` count in each direction.

### `blame` ✅
- **In:** `path`, `rev = HEAD`, `line_range?: [start, end]`.
- **Out:** `hunks[] {start_line, lines, commit_id, author, summary, original_path}`.
- **gix:** `repo.blame_file` (the `blame` feature is already enabled).
- **Tests:** compare with `git blame --porcelain` on the fixture.

**Exit criteria:** an agent can do "what did I change, why, and where did this line come from" entirely through MCP. Every output is checked against the git CLI.

### Phase 1: deviations from the plan

- **Across tools.**
  - Every list takes `limit` (and `skip` where paging makes sense) and reports `truncated`, as the design principles require; the specs above didn't always list these.
  - The server now changes to the repository root at startup. gix resolves pathspecs against the working directory captured when a repo is opened, so without this, launching from a subdirectory would shift every path argument.
- **`status`.**
  - `upstream` reports the configured tracking ref from local config (e.g. `refs/remotes/origin/main`) instead of always `null`. It reads only local data, so the local-only promise still holds.
  - Added `ignored`, `counts`, `truncated`, `clean` and `limit`. `copied` is reported when copy detection is configured.
  - The HEAD↔index side reuses `diff`'s staged comparison, so both tools apply the same rename settings and gix workarounds.
- **`diff`.**
  - `from` alone (revision ↔ worktree, `git diff <rev>`) is refused with an explanation; `staged: true` with `from` compares a revision with the index instead. Revision ↔ worktree needs a combined tree/index/worktree walk and can come later.
  - Added `mode`, `unmerged`, `limit`/`skip`, per-file `binary` and `patch_truncated`.
  - Patches are rendered by our own hunk writer, so they match `git diff` byte for byte: compact hunk ranges, git's default function context in `@@` headers, `\ No newline at end of file`, and `Subproject commit` lines for submodules. A submodule that is only dirty (no new commit) isn't reported, as with `git diff --ignore-submodules=dirty`.
  - The first patch that overflows `max_bytes` is cut at a line boundary rather than dropped.
- **`read_file`.**
  - Added `mode`, `path` and `next_offset`, and chunks never split a UTF-8 character.
  - Worktree reads refuse `.git/` and paths that escape through a symlinked directory. A symlink is returned as its target, never followed.
- **`list_tree`.** Recursive listings match `git ls-tree -r`: no directory entries. Added `skip` and `total`.
- **`rev_parse`.** Besides `single`, `range` and `merge_base_range`, it reports `exclude` (`^a`), `include_only_parents` (`a^@`) and `exclude_parents` (`a^!`). `merge_base` is filled in for `a...b`.
- **`file_history`.**
  - Implements git's default history simplification rather than listing every commit whose tree entry differs, so results match plain `git log -- <path>`.
  - `follow_renames` shipped now rather than later, and matches `git log --follow` on the tests' rename scenarios.
- **`search_commits`.**
  - `author` is a regex over `Name <email>`, like `git log --author`.
  - Dates use gix-date, so relative forms such as `2 weeks ago` work.
  - A `path` filter uses the same simplified walk as `file_history`, and `skip` was added.
- **`compare_refs`.** Added `limit` and `truncated`.
- **`blame`.** Added `line_count`, `original_start_line` and `limit`. Renames are followed by default, as in `git blame`.

---

## Phase 2: Deep read and inspection ✅ (done)

All 14 tools shipped, each tested against the git CLI (`git describe`, `git tag`, `git log -g`, `git stash list`, `git notes`, `git grep`, `git check-ignore`, `git check-attr`, `git config --list --show-scope`, `git worktree list --porcelain`, `git submodule status`, `git count-objects -v`, `git ls-files -u`, `git merge-tree --write-tree`). Where they differ from the table below, the reasons are in [Phase 2: deviations from the plan](#phase-2-deviations-from-the-plan).

| Tool | In → Out | gix |
|---|---|---|
| `grep` ✅ | `pattern` (regex), `rev \| worktree`, `pathspec`, `ignore_case`, `limit` → `matches[] {path, line, text}` | tree traversal + blob read; worktree via `dirwalk` honoring excludes |
| `describe` ✅ | `rev` → `v1.2-3-gabc123` + parts | `commit.describe()` |
| `show_tag` ✅ | `name` → tagger, message, target, kind | `find_reference` then `peel` |
| `reflog` ✅ | `ref = HEAD`, `limit` → entries `{old, new, committer, message}` | `reference.log_iter()` |
| `list_stashes` ✅ | → entries from `refs/stash` reflog | reflog read only; gix has no stash API |
| `list_worktrees` ✅ | → `{path, branch, head, locked, prunable}` | `repo.worktrees()` |
| `list_submodules` ✅ | → `{name, path, url, recorded_commit, checked_out_commit, state}` | `repo.submodules()` |
| `conflicts` ✅ | → during a merge/rebase: `path`, ours/theirs/base blob ids and optional contents | index stages 1–3 via `open_index()` |
| `check_ignore` ✅ | `paths[]` → matching rule and source file | `repo.excludes(...)` |
| `check_attr` ✅ | `paths[]`, `attrs[]` → values | `repo.attributes(...)` |
| `config_get` ✅ | `key` or `section` → values with scope (local/global/system) | `config_snapshot()`, values redacted: `credential.*`, `*.token`, `*password*`, `http.*.extraheader` |
| `merge_preview` ✅ | `ours`, `theirs` → `clean: bool`, `conflicts[] {path, kind}`, resulting tree id | `merge_commits` in memory only. Nothing is written, so it is read-only and safe |
| `notes` ✅ | `rev` → notes text | `repo.notes()` |
| `count_objects` ✅ | → loose/packed counts and sizes, commit-graph present? | odb stats |

**Exit criteria:** code-review and archaeology workflows (“is it safe to merge X into Y?”, “who touches this module?”) work without write access.

### Phase 2: deviations from the plan

- **Across tools.** `server.rs` was split into one router per area before the first Phase 2 tool landed. The docs examples, Phase 1's included, now come from `scripts/demo-repo.sh`, which builds the demo repository with fixed dates so its SHAs are reproducible. It recreates the original hand-built demo's files (adding an annotated tag, a note and a stash), so only SHAs changed in the existing examples.
- **`describe`.**
  - When no tag is reachable it returns the short SHA with `tag: null`, like `git describe --always`, instead of failing.
  - Added `tags`, `long` and `first_parent`. `--all` isn't offered: gix names branch candidates `main` where git prints `heads/main`, so the output wouldn't match git.
- **`show_tag`.** Added `peeled`/`peeled_type` (the object after peeling every tag layer) and `signed`. Signatures are detected, not verified.
- **`reflog`.** Each entry has a `selector` (`main@{2}`) that other tools accept as a revision. `old_id` is `null` for the entry that created the ref. Added `skip`.
- **`list_stashes`.** Each stash reports its `base` commit, `branch` (parsed from the message), `includes_untracked`, and a `selector`. Paginated with `limit`/`skip`.
- **`grep`.**
  - Searches tracked files only, like `git grep` without `--untracked`, reading them from disk; `index` searches staged content. The planned `dirwalk` over untracked files wasn't needed for that.
  - Skips binary files, symlinks and submodules like `git grep -I`. Each match reports a byte `column` (`--column`), and its `text` is cut at 512 bytes.
  - Added `fixed_strings` and `skip`. Patterns use Rust regex syntax rather than git's POSIX basic regex.
- **`check_ignore`.**
  - Every path gets an answer (`--non-matching`) with `ignored`, `tracked`, and the deciding rule's pattern, file and line.
  - `ignored` follows git's default (tracked files are never ignored), while `pattern` shows the rule even for tracked files, as `--no-index` does.
  - A parent directory's rule decides for everything below it, as in git; gix only matches the path itself, so the tool checks parents first. At most 1000 paths per call.
- **`check_attr`.** Reports each attribute as `set`, `unset`, `value` or `unspecified`, and `attrs: []` lists everything like `-a`. In that case the order may differ from git's, which lists macros first. At most 1000 paths per call.
- **`conflicts`.**
  - Reports each file's `conflict` kind (the `git status` codes) plus `base`, `ours` and `theirs` versions, and the operation `in_progress`.
  - Text is optional (`include_content`) under one byte budget for the whole response, rather than always included. Paginated.
- **`merge_preview`.**
  - The resulting `tree` matches `git merge-tree --write-tree` byte for byte, conflict markers included, also for criss-cross histories (several merge bases).
  - Conflicts are reported as index stages in the same `ConflictFile` shape as `conflicts`, instead of `{path, kind}`. Added `merge_bases`, `up_to_date`, `fast_forward` and `allow_unrelated`; `ours` defaults to `HEAD`.
  - gix writes merged blobs, trees and virtual merge bases as the merge runs, so the merge runs against an in-memory object store (`with_object_memory`). A test proves `.git/objects` is unchanged afterwards. Merge drivers and filters from the user's config still run, as with git.
- **`list_worktrees`.** Besides path, HEAD, branch, locked and prunable, each entry has `id`, `is_main`, `is_bare`, `detached`, `lock_reason` and `is_current` (the worktree being served). Paginated.
- **`list_submodules`.** `recorded_commit` is the commit the index records, as `git submodule status` uses; `state` mirrors its prefixes (`uninitialized`, `up_to_date`, `different_commit`, `conflicted`). Added `branch`. URLs get the same password redaction as `config_get`.
- **`config_get`.**
  - Takes `key`, `section` (with or without a subsection), or neither to list everything, plus a `scope` filter and pagination. Scopes use git's names: `system`, `global`, `local`, `worktree`, `command`.
  - Redaction also covers keys containing `passwd` or `secret`, and the password in any URL value.
  - gix turns environment variables such as `GIT_EDITOR` into configuration values; those are left out, matching `git config --list`.
- **`count_objects`.** Sizes are in bytes rather than KiB. Added `multi_pack_index` and an `alternates` count; git's `prune-packable` and `garbage` counts aren't reported.
- **`notes`.** Added `ref` (a notes ref, expanded like `git notes --ref`) and `max_bytes`. `rev` isn't peeled, so a tag's notes are those on the tag object, as in `git notes show`.

---

## Phase 3: Safe writes (behind `--allow-write`)

Shared guarantees for every tool in this phase:
- **Refusals.** Refuse if `repo.state()` reports an operation in progress (merge, rebase, …), unless the tool is that operation's continuation.
- **Locking and atomicity.** Use gix lock files (`gix-lock`). Ref updates go through `edit_references` with `PreviousValue::MustExistAndMatch` (compare-and-swap) to avoid races with concurrent human edits.
- **Output.** Return the new state (new commit id, new index summary) so the agent doesn't need a follow-up `status` call.
- **Annotations.** `read_only_hint = false`. Set `destructive_hint` where data can be lost.

| Tool | Behaviour | gix | Notes |
|---|---|---|---|
| `stage` | `paths[]` / `all` → add to index (adds, modifications, deletions) | build an index from the status iterator, `write_blob`, `index.write()` | respect `.gitattributes` filters (`filter` pipeline) and executable bits |
| `unstage` | `paths[]` → reset index entries to HEAD | rewrite entries from the HEAD tree | |
| `commit` | `message`, `amend = false`, `allow_empty = false`, `author?` → new commit | `index.write_tree` + `repo.commit(...)` / `commit_as` | **gap:** hooks (`pre-commit`, `commit-msg`) and signing aren't run by gix. Either document this or optionally shell out to the hooks (opt-in `--run-hooks`) |
| `create_branch` | `name`, `start_point = HEAD`, `force = false` | `edit_reference` with `MustNotExist` | name validated by `gix-validate` |
| `delete_branch` | `name`, `force = false`; non-force refuses unmerged branches | `delete_local_branches_if_unchanged` | `destructive_hint`, `dry_run` default |
| `rename_branch` | `old`, `new` | two ref edits + reflog move | |
| `create_tag` | `name`, `target`, `message?` (annotated if set) | `repo.tag(...)` / `tag_reference` | |
| `switch` | `branch` or `rev` (detached), `create?` | update HEAD, then check out the tree with `checkout_options` + `gix-worktree-state` | refuse if the worktree has changes that would be overwritten (compute via status ∩ changed paths) |
| `restore` | `paths[]`, `source = index \| HEAD \| rev`, `staged` | per-file checkout | `destructive_hint`, `dry_run` default |
| `reset` | `mode: soft \| mixed` (no `hard` in this phase), `rev` | move the branch ref, rebuild the index for `mixed` | `hard` waits until Phase 4 with `dry_run` |

**Exit criteria:** an agent can run the full local edit, stage, commit and branch loop. Every write tool has a test proving it refuses on a dirty or conflicting state and leaves the repo unchanged on refusal.

---

## Phase 4: History rewriting and multi-step operations

gix has no sequencer, so this phase is where we build our own logic on its primitives.

| Tool | Approach |
|---|---|
| `merge` | `merge_commits` → if clean, write the tree, create a merge commit, check out. If there are conflicts, write conflict stages to the index plus markers in the worktree and set `MERGE_HEAD`. Then `merge_continue` / `merge_abort` |
| `cherry_pick` / `revert` | `merge_trees(base=parent, ours=HEAD, theirs=commit)` per commit (inverted for revert). Write `CHERRY_PICK_HEAD` on conflict |
| `rebase` | Our own minimal sequencer: a list of picks in `.git/rebase-merge` (git-compatible layout, so `git rebase --continue` still works). Supports `onto`, `continue`, `abort` and `skip`. No interactive editor: `todo` comes in as a structured list (`pick`/`drop`/`squash`/`reword` with message) |
| `stash_push` / `stash_pop` / `stash_drop` | Implement git's stash commit layout (worktree commit with index and untracked parents) on `commit`/`write_tree`. **gap:** no gix support |
| `reset` mode `hard` | With `dry_run`, it lists files that would lose changes |
| `worktree_add` / `worktree_remove` | `.git/worktrees/<name>` layout and checkout |
| `clean` | `dirwalk` untracked/ignored → `dry_run` lists them by default |

**Exit criteria:** each operation leaves on-disk state that real `git` can continue or abort. Test this by interleaving gitrs-mcp and git CLI steps.

---

## Phase 5: MCP-native extras

- **Resources.**
  - `gitrs://commit/{id}`, `gitrs://file/{rev}/{path}`, `gitrs://diff/{from}..{to}`, so clients can attach repo content as context.
  - Resource templates plus `completion` for revs and paths.
- **Prompts.**
  - `write_commit_message`: fills in the staged diff.
  - `review_changes`: diff plus surrounding blame.
  - `explain_history`: file history for a path.
- **Progress notifications** for long operations (`blame` on big files, `grep`, large `log`). Honour cancellation via `gix::interrupt` / `should_interrupt` flags.
- **Change notifications.** Watch `.git/HEAD`, refs and the index, and emit `notifications/resources/updated`. This is optional behind `--watch`.
- **Multi-repo.**
  - Accept multiple `--repo` values, or use MCP `roots` from the client.
  - Every tool gets an optional `repo` param. Refuse paths outside the declared roots.
- **Caching.** Keep a per-process object cache (`object_cache_size_if_unset`) and reuse the commit-graph. Benchmark `log` and `blame` against git on git.git itself.

---

## Phase 6: Hardening and release

- **Security review.**
  - Paths: no `..` escapes, no following symlinks out of the worktree, and `core.worktree` overrides honoured safely.
  - Ownership checks: gix's `git_dir_trust`, the equivalent of git's `safe.directory`.
  - Redaction: config values with secrets are redacted.
- **Fuzzing** of argument parsing and revspecs (`cargo fuzz`), plus large-repo stress tests (git.git, linux.git).
- **Integration tests** through a real MCP client (`rmcp` `client` + `transport-child-process` features) that spawns the binary.
- **Distribution.**
  - `cargo install gitrs-mcp`, prebuilt binaries via `cargo-dist`, a Homebrew tap.
  - Docs with copy-paste client config for Claude Code (`claude mcp add gitrs -- gitrs-mcp --repo .`), Claude Desktop, VS Code and Cursor.
- **Versioning.** Tool names and schemas are a public API. Use semver: add fields freely, and never rename or remove fields within a major version.

---

## Known gix gaps (as of 0.90) and plan

| Missing in gix | Impact | Plan |
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
| Unified diff output differs from git (found in Phase 1) | gix's unified-diff helpers print `,1` ranges and no function context, and its default line input strips terminators, which loses `\ No newline at end of file` | Own hunk writer in `tools/diff.rs` on top of gix's line diff |

## Milestone summary

| Phase | Tools | Rough size | Status |
|---|---|---|---|
| 0 Scaffold | 4 | — | ✅ done |
| 1 Core read | 9 | ~4.9k lines including tests (planned ~2–3k) | ✅ done |
| 2 Deep read | 14 | ~2k LOC | ✅ done |
| 3 Safe writes | 10 | ~2–3k LOC (index writing and checkout safety are the hard parts) | ⏭️ next |
| 4 Rewriting | 8 | ~3–4k LOC (sequencer and stash) | planned |
| 5 MCP extras | resources, prompts, progress, multi-repo | ~1.5k LOC | planned |
| 6 Hardening | — | ongoing | planned |
