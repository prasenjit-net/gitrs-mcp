# gitrs-mcp roadmap

A local-only Git MCP server in Rust on top of [gitoxide](https://github.com/GitoxideLabs/gitoxide) (`gix` 0.90) and the official MCP SDK (`rmcp` 3.7), speaking MCP over stdio.

## Status

_Last updated 2026-10-27._

| Phase | Status |
|---|---|
| 0 Scaffold | ✅ Done: 4 tools |
| 1 Core read | ✅ Done: 9 tools, each checked against the git CLI |
| 2 Deep read and inspection | ✅ Done: 14 tools, each checked against the git CLI |
| 3 Safe writes | ✅ Done: 10 tools behind `--allow-write`, each checked against the git CLI |
| 4 History rewriting | ✅ Done: 12 tools behind `--allow-write`, each checked against the git CLI, with git and gitrs-mcp able to continue each other's operations |
| 5 MCP-native extras | ✅ Done: resources, prompts, completion, progress and cancellation, `--watch`, several repositories, caching and benchmarks |
| 6 Hardening and release | Planned |

The server has 28 read-only tools, plus 22 write tools registered only with `--allow-write`, along with resources, prompts and completion. 262 unit tests and the end-to-end smoke test pass, with 96.7% line coverage.

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
   ├─ write.rs      guards shared by write tools
   ├─ branches.rs   create_branch, delete_branch, rename_branch, create_tag
   ├─ staging.rs    stage, unstage
   ├─ commit.rs     commit, reset
   ├─ checkout.rs   restore, switch
   ├─ clean.rs      clean
   ├─ stash.rs      stash_push, stash_pop, stash_drop
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

## Phase 3: Safe writes (behind `--allow-write`) ✅ (done)

All 10 tools shipped. Each is tested against git on twin repositories (`git ls-files -s`, `git status --porcelain=v2`, working-tree files, refs and reflog messages must match), and each has tests proving that refused and dry-run calls leave the repository byte for byte unchanged. Where they differ from the plan below, the reasons are in [Phase 3: deviations from the plan](#phase-3-deviations-from-the-plan).

Shared guarantees for every tool in this phase:
- **Refusals.** Refuse if `repo.state()` reports an operation in progress (merge, rebase, …), unless the tool is that operation's continuation.
- **Locking and atomicity.** Use gix lock files (`gix-lock`). Ref updates go through `edit_references` with `PreviousValue::MustExistAndMatch` (compare-and-swap) to avoid races with concurrent human edits.
- **Output.** Return the new state (new commit id, new index summary) so the agent doesn't need a follow-up `status` call.
- **Annotations.** `read_only_hint = false`. Set `destructive_hint` where data can be lost.

| Tool | Behaviour | gix | Notes |
|---|---|---|---|
| `stage` ✅ | `paths[]` / `all` → add to index (adds, modifications, deletions) | build an index from the status iterator, `write_blob`, `index.write()` | respect `.gitattributes` filters (`filter` pipeline) and executable bits |
| `unstage` ✅ | `paths[]` → reset index entries to HEAD | rewrite entries from the HEAD tree | |
| `commit` ✅ | `message`, `amend = false`, `allow_empty = false`, `author?` → new commit | `index.write_tree` + `repo.commit(...)` / `commit_as` | **gap:** hooks (`pre-commit`, `commit-msg`) and signing aren't run by gix. Either document this or optionally shell out to the hooks (opt-in `--run-hooks`) |
| `create_branch` ✅ | `name`, `start_point = HEAD`, `force = false` | `edit_reference` with `MustNotExist` | name validated by `gix-validate` |
| `delete_branch` ✅ | `name`, `force = false`; non-force refuses unmerged branches | `delete_local_branches_if_unchanged` | `destructive_hint`, `dry_run` default |
| `rename_branch` ✅ | `old`, `new` | two ref edits + reflog move | |
| `create_tag` ✅ | `name`, `target`, `message?` (annotated if set) | `repo.tag(...)` / `tag_reference` | |
| `switch` ✅ | `branch` or `rev` (detached), `create?` | update HEAD, then check out the tree with `checkout_options` + `gix-worktree-state` | refuse if the worktree has changes that would be overwritten (compute via status ∩ changed paths) |
| `restore` ✅ | `paths[]`, `source = index \| HEAD \| rev`, `staged` | per-file checkout | `destructive_hint`, `dry_run` default |
| `reset` ✅ | `mode: soft \| mixed` (no `hard` in this phase), `rev` | move the branch ref, rebuild the index for `mixed` | `hard` waits until Phase 4 with `dry_run` |

**Exit criteria:** an agent can run the full local edit, stage, commit and branch loop. Every write tool has a test proving it refuses on a dirty or conflicting state and leaves the repo unchanged on refusal.

### Phase 3: deviations from the plan

- **Across tools.**
  - Write tools are a separate router (`server/write.rs`) added only with `--allow-write`. `make smoke` runs a second session with the flag and checks they appear only there.
  - The in-progress refusal applies to tools that move HEAD, rewrite the index or touch the worktree, and to `rename_branch`. Creating branches and tags, and deleting a branch that isn't checked out, stay allowed during an operation, as in git.
  - A committer identity is required from config (or `GIT_COMMITTER_*`); gix's generic fallback identity is never used.
- **`create_branch`.** Reflog messages match git (`branch: Created from <start>`, `branch: Reset to <start>`); forcing over a checked-out branch is refused.
- **`delete_branch`.** Uses gix's `delete_local_branches_if_unchanged`, which also removes `branch.<name>.*` config. The merged check follows `git branch -d`: the configured upstream when it exists locally, else HEAD. A dry run performs every check the real deletion does.
- **`rename_branch`.** gix has no rename, so it's one ref transaction (create the new name, delete the old one, repoint HEAD when needed), with the reflog copied over first and the `branch.<name>` config section renamed under the config lock afterwards. Added `force` and `replaced`.
- **`stage`.**
  - Works from the index-to-worktree status, then hashes each file through gix's filter pipeline, so it matches `git add` (or `-A`) for adds, deletions, symlinks, the executable bit (`core.fileMode`), CRLF conversion, submodule commits and conflict resolution.
  - Ignored files named explicitly and nested repositories are reported in `skipped`, rather than failing the call as `git add` does.
  - Allowed during an in-progress operation, since that's how conflicts get resolved.
- **`unstage`.**
  - Takes `paths` or `all`. Restored entries get a zero stat, so git re-reads the file rather than trusting a stat that matched the replaced entry.
  - Refused during an in-progress operation.
- **`commit`.**
  - Hooks aren't run, and `--run-hooks` wasn't added. Instead `hooks_skipped` reports the commit hooks present (`core.hooksPath` or `.git/hooks`), so the agent knows checks were skipped.
  - With `commit.gpgSign` set, it refuses rather than creating unsigned commits; `create_tag` does the same for annotated tags with `tag.gpgSign`.
  - The tree is built from the index (intent-to-add entries left out) in an in-memory object store first, so refusals such as "nothing to commit" write no objects. The index lock is held throughout, as `git commit` does.
  - Reflog messages match git (`commit:`, `commit (initial):`, `commit (amend):`). `amend` keeps the original author unless `author` is given.
- **`reset`.**
  - `rev` defaults to `HEAD`. A mixed reset moves HEAD inside the index update, so if the ref can't be moved the index stays as it was.
  - A soft reset works in a bare repository and refuses an index with unresolved conflicts, as git does. Not marked destructive: the working tree is never touched, and `previous` undoes it.
- **`restore`.**
  - Follows `git restore`: the worktree from the index by default, the index from `HEAD` with `staged`, and tracked files missing from a revision `source` are deleted.
  - A dry run by default, like the other destructive tools; each file reports `discards_changes` when its current content isn't in HEAD or the index.
  - Paths with conflict stages are refused instead of offering `--ours`/`--theirs`/`--merge`.
- **`switch`.**
  - Arguments are `target` plus `create`, `start_point` and `detach`. Instead of a full checkout of the new tree, it applies only the paths that differ between the two commits (git's two-way merge), so local changes elsewhere are carried over. Any path it would overwrite with local changes, or with an untracked file in the way, refuses the whole switch and names the files. Ignored files in the way are refused too, where git would overwrite them.
  - Submodule commits are updated in the index only, as without `--recurse-submodules`.
  - Remote-tracking branches aren't guessed (`git switch <name>` creating a branch from `origin/<name>`); use `create` with `start_point`.
- **Reflogs.** gix's ref transactions don't log changes to HEAD's symbolic target, so `switch` and `rename_branch` append git's HEAD reflog entry (`checkout: moving from … to …`, `Branch: renamed …`) themselves.
- **Index writes.**
  - gix's `File::write` takes the lock only when writing and keeps a stale cache-tree. `tools::write::update_index` locks before reading instead, and writes only the end-of-index-entry extension. A split index arrives merged with its shared index and is written back whole; a sparse index is refused.
- **`create_tag`.** Added `force`; messages get git's whitespace cleanup. The result reuses `show_tag`'s shape. No reflog is written for tags, as in git.

---

## Phase 4: History rewriting and multi-step operations

gix has no sequencer, so this phase is where we build our own logic on its primitives.

| Tool | Approach |
|---|---|
| `merge` ✅ | `merge_commits` → if clean, write the tree, create a merge commit, check out. If there are conflicts, write conflict stages to the index plus markers in the worktree and set `MERGE_HEAD`. Then `merge_continue` / `merge_abort` |
| `cherry_pick` / `revert` ✅ | `merge_trees(base=parent, ours=HEAD, theirs=commit)` per commit (inverted for revert). Write `CHERRY_PICK_HEAD` on conflict |
| `rebase` ✅ | Our own minimal sequencer: a list of picks in `.git/rebase-merge` (git-compatible layout, so `git rebase --continue` still works). Supports `onto`, `continue`, `abort` and `skip`. No interactive editor: `todo` comes in as a structured list (`pick`/`drop`/`squash`/`reword` with message) |
| `stash_push` / `stash_pop` / `stash_drop` ✅ | Implement git's stash commit layout (worktree commit with index and untracked parents) on `commit`/`write_tree`. **gap:** no gix support |
| `reset` mode `hard` ✅ | With `dry_run`, it lists files that would lose changes |
| `worktree_add` / `worktree_remove` ✅ | `.git/worktrees/<name>` layout and checkout |
| `clean` ✅ | `dirwalk` untracked/ignored → `dry_run` lists them by default |

**Exit criteria:** each operation leaves on-disk state that real `git` can continue or abort. Test this by interleaving gitrs-mcp and git CLI steps.

### Phase 4: deviations from the plan

- **`reset` mode `hard`.**
  - Reuses `restore`'s planner (index and worktree from the target commit, everything matched), and overwrites conflict stages as `git reset --hard` does.
  - `dry_run` defaults to true for `hard` only, so `soft` and `mixed` keep applying immediately. The result gains `files`, `files_count` and `discards_changes`, and `reset` is now annotated destructive.
  - Like the other HEAD-moving tools, it refuses during an in-progress operation; `abort_operation` covers aborting.
  - gix skips a ref update that leaves the ref where it is, but git still logs `reset: moving to HEAD`, so `reset` appends those reflog entries itself.
- **Stashes.**
  - `stash_push` writes git's layout (a `W` commit with HEAD, the index commit and an optional untracked-files commit as parents, and git's messages byte for byte). Tests check git can pop our stashes and we can pop git's. Pathspec-limited stashes (`git stash push -- <paths>`) aren't supported.
  - The stash is built in an in-memory object store first, and the reset to HEAD is planned, before anything is written; a refusal leaves no stash behind.
  - `stash_pop` merges the stash onto HEAD in memory. If that conflicts, or would touch a file with local changes or an untracked file in the way, it refuses instead of leaving conflict markers; git would apply what it can. `drop: false` gives `git stash apply`, and `restore_index` gives `--index`.
  - `stash_drop` is a dry run by default. gix can't delete one reflog entry, so the `refs/stash` reflog is rewritten under its lock.
- **Submodules in `restore` and `reset --hard`.** Found while building stashes: restoring `.` in a repository with submodules refused because each submodule's directory "was in the way". Gitlinks now only change in the index, and their checkouts are never touched, as in git.
- **`clean`.** Follows `git clean`: files only by default; `directories` (`-d`), `ignored: also` (`-x`) and `only` (`-X`). Nested repositories are always kept (git needs `-ff` to remove them). A dry run by default.
- **Continuing and aborting.**
  - Planned as `merge_continue` / `merge_abort` plus rebase `continue`/`abort`/`skip`. Shipped instead as `continue_operation` (with `skip` for rebases) and `abort_operation`, which work out from the repository state what's in progress. This also covers cherry-picks and reverts, and operations git started.
  - `abort_operation` isn't annotated destructive. It discards only conflict resolutions in progress, like `git merge --abort`, and local changes to files the operation didn't touch are kept.
  - Each operation writes git's state files byte for byte where git reads them back (`MERGE_MSG` with its `# Conflicts:` list, `MERGE_MODE`, `ORIG_HEAD`, `AUTO_MERGE` removal), plus git's reflog messages. Tests finish gitrs-mcp's operations with git and git's with gitrs-mcp.
- **`merge`, `cherry_pick` and `revert`.**
  - The merge is computed and checked in an in-memory object store first, then repeated for real under the index lock, so a refusal writes nothing.
  - Like git's merge machinery, they refuse when the index has staged changes. Unstaged changes are kept, unless the merge would rewrite that file, in which case the whole operation refuses and names it.
  - One revision per call; ranges and merge commits (`-m`) aren't supported. Merging takes one branch (no octopus), with git's default strategy (`ort`) and no strategy options. A cherry-pick whose change is already in HEAD is refused, as git does without `--allow-empty`.
- **`rebase`.**
  - It's a sequencer of our own on gix's tree merge, keeping its state in git's `.git/rebase-merge` layout (`git-rebase-todo`, `done`, `msgnum`, `end`, `author-script`, `stopped-sha`, `REBASE_HEAD`). So `git rebase --continue`, `--skip` and `--abort` work on our rebases, and `continue_operation` works on git's when every remaining step is one we support.
  - Messages given for `reword` and `squash` steps are kept in an extra `gitrs-messages.json`, which git ignores. If git continues such a rebase, it uses the original messages.
  - `exec`, `break`, `edit`, `label`, `reset` and `merge` steps aren't supported; a git rebase containing them is left for git to continue. Merge commits are left out (no `--rebase-merges`), and commits that become empty are dropped, as git does by default.
  - The working tree and index must be clean (no `--autostash`), and HEAD must be on a branch.
  - It doesn't use `--fork-point` or the configured upstream: `onto` is required, and `upstream` defaults to it.
- **Worktrees.** `worktree_add` uses gix's `add_worktree`, and `worktree_remove` uses `prepare_remove_worktree`. A branch created for a new worktree is deleted again if the checkout fails. `worktree_remove` refuses the main worktree and the one being served. It's a dry run by default, and `force` covers both dirty and locked worktrees (`--force --force`).

---

## Phase 5: MCP-native extras ✅

- **Resources.** ✅
  - `gitrs://commit/{id}`, `gitrs://file/{rev}/{path}`, `gitrs://diff/{from}..{to}`, so clients can attach repo content as context.
  - Resource templates plus `completion` for revs and paths.
- **Prompts.** ✅
  - `write_commit_message`: fills in the staged diff.
  - `review_changes`: diff plus surrounding blame.
  - `explain_history`: file history for a path.
- **Progress notifications** ✅ for long operations (`blame` on big files, `grep`, large `log`). Honour cancellation via `gix::interrupt` / `should_interrupt` flags.
- **Change notifications.** ✅ Watch `.git/HEAD`, refs and the index, and emit `notifications/resources/updated`. This is optional behind `--watch`.
- **Multi-repo.** ✅
  - Accept multiple `--repo` values, or use MCP `roots` from the client.
  - Every tool gets an optional `repo` param. Refuse paths outside the declared roots.
- **Caching.** ✅ Keep a per-process object cache (`object_cache_size_if_unset`) and reuse the commit-graph. Benchmark `log` and `blame` against git on git.git itself.

### Phase 5: deviations from the plan

- **Resources.**
  - The commit template takes `{rev}` rather than `{id}`, since any revision works. The templates are `gitrs://commit/{rev}`, `gitrs://file/{rev}/{+path}` (RFC 6570 reserved expansion, so the path keeps its `/`) and `gitrs://diff/{from}..{to}`. A `/` inside a revision is percent-encoded as `%2F`, since the URI can't tell it apart from a path separator otherwise.
  - Three fixed resources are listed as well, so clients that don't expand templates still get something to attach: the HEAD commit, staged changes (`diff/HEAD..index`) and unstaged changes (`diff/index..worktree`).
  - Content is text, not JSON. A commit reads like `git show` (and the tests compare it with `git show`), and diffs are `git diff` patches, since that's what a model reads best as attached context. Both are built on the `show_commit`, `diff` and `read_file` tools.
  - A merge commit's patch is against its first parent, not git's combined diff, so that it applies.
  - `file` takes `index` and `worktree` as revisions, like `read_file`. `diff` supports `<rev>..<rev>`, `<rev>..index` and `index..worktree`; a revision against the working tree isn't supported, as in the `diff` tool.
  - Resources can't be paged. Files over 1 MiB are refused, with a pointer to `read_file`. Diffs stop at 1,000 files and 1 MiB of patch text, with a closing note saying what was left out.
- **Prompts.**
  - Each prompt returns one user message holding the instructions and the data, with code inside Markdown fences longer than any backtick run in it. Prompt arguments are strings in MCP, so `explain_history`'s `limit` is parsed and refused with a clear message when it isn't a positive number.
  - `write_commit_message` also lists the last 10 commit subjects as a style guide, and refuses when nothing is staged.
  - `review_changes` takes `from`/`to` with the diff resource's pairs (default the staged changes). "Surrounding blame" became the blame of each hunk's old lines, context included, at `from`, or at HEAD for `index..worktree`. Pure additions have nothing to blame and are skipped.
  - `explain_history` follows renames and includes the full message and patch of the 10 most recent commits, within a 128 KiB patch budget.
  - Found while testing: `gitrs://diff/HEAD..index` failed before the first commit. HEAD now goes to the diff tool's default, which treats an unborn HEAD as the empty tree.
- **Progress and cancellation.**
  - `log`, `search_commits`, `file_history` and `grep` report progress and stop on cancellation, through a `tools::progress::Watch` that wraps their iterators. `search_commits` counts commits scanned, not matches, since a selective filter can scan far more than it returns.
  - Cancellation uses our own `AtomicBool`, set when the request is cancelled, rather than `gix::interrupt`. The process-wide interrupt would cancel every concurrent call, and the walks are our own loops anyway.
  - `blame` only reports when it starts: gix's `blame_file` has no progress or interrupt hook, so a cancelled blame stops only once it returns.
  - Reports are throttled to one every 250 ms, so quick calls send none. All notifications are sent before the result.
  - The tool functions gained a `watch: &Watch` parameter. Other callers, such as the prompts, pass `Watch::none()`.
- **Change notifications.**
  - Instead of watching `.git/HEAD`, refs and the index with a file watcher, a polling task (once a second) checks a fingerprint of each *subscribed* resource. It needs no new dependency and costs nothing without subscriptions, and it also catches changes a file watcher on `.git` would miss: edits on disk for `worktree` resources, and revisions that start or stop resolving.
  - Both subscription styles are served: legacy `resources/subscribe`/`unsubscribe`, which current clients send, and `subscriptions/listen` from protocol 2026-07-28, which rmcp 3.7 implements.
  - Found while testing: rmcp acknowledges a `subscriptions/listen` before calling the handler, so a change right after the acknowledgment was missed. Baselines are now taken in `accepted_subscription_filter`, before the acknowledgment.
- **Multi-repo.**
  - `--repo` can be repeated. The first is the default, so single-repository use is unchanged. Repositories at the client's MCP roots are added after initialization and follow `roots/list_changed`. Roots are deprecated by protocol 2026-07-28, but today's clients send them.
  - A root is served only when the repository found there has its root inside the declared directory, so a root inside a larger repository doesn't expose the rest of it. `repo` accepts a served repository's name or a path inside one. Relative paths and `..` are refused.
  - The optional `repo` argument is added in the server layer (`InRepo<T>` flattens the tool's own arguments), so the tool functions stay single-repository and their schemas gain one property. Resources take `?repo=<name>` (templates end in `{?repo}`), and prompts and completion take `repo` too.
  - Added a `list_repos` tool, so the model can discover the names.
  - The server no longer makes the repository root its working directory, which can't work for several repositories. It moves to the filesystem root before opening any. From outside every working tree, gix resolves each repository's paths from its root.
  - Found while switching: `worktree_add`'s `path` and `worktree_remove`'s `worktree` resolved relative paths against the process directory instead of the repository root. Both now go through `tools::root_relative`.
- **Caching and benchmarks.**
  - Each blocking-pool thread keeps its thread-local `gix::Repository` per handle between calls, with a 16 MiB object cache (`object_cache_size_if_unset`). Before, every call started from a fresh instance. gix already uses the commit-graph on every walk, so there was nothing extra to keep.
  - `make bench` (`scripts/bench.py`) times tool calls against the equivalent git commands. The results on git.git are in [docs/development.md](docs/development.md#benchmarks). The cache makes repeated calls about 10% faster.
  - Found by the benchmark: `log` built a log entry, including a unique short id, for every *skipped* commit, so `skip: 20000` took 754 ms against git's 96 ms. `tools::paginate_built` only builds the returned page; it's now 84 ms. `file_history` and `search_commits` use it too.
  - Remaining gaps: `file_history` is about 1.7 times git (our own walk, no changed-path Bloom filter), and `blame` about 1.5 times (gix's algorithm). Phase 6 can revisit them.
- **Completion.** Prompt arguments complete too: `rev`, `from` and `to` complete from `HEAD` and ref short names, plus `index`/`worktree` where they're valid. `path` completes one directory level at a time against the context revision. Completion errors give no values, since a half-typed context revision is normal.

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
| Index writes keep a stale cache-tree (found in Phase 3) | `gix_index::File::write` writes the `TREE` extension unchanged after entries change, so a later `git commit` could record outdated subtrees ([gitoxide#2421](https://github.com/GitoxideLabs/gitoxide/issues/2421)) | `update_index` writes only the end-of-index-entry extension; git rebuilds the rest |
| Unified diff output differs from git (found in Phase 1) | gix's unified-diff helpers print `,1` ranges and no function context, and its default line input strips terminators, which loses `\ No newline at end of file` | Own hunk writer in `tools/diff.rs` on top of gix's line diff |

## Milestone summary

| Phase | Tools | Rough size | Status |
|---|---|---|---|
| 0 Scaffold | 4 | — | ✅ done |
| 1 Core read | 9 | ~4.9k lines including tests (planned ~2–3k) | ✅ done |
| 2 Deep read | 14 | ~2k LOC | ✅ done |
| 3 Safe writes | 10 | ~2–3k LOC (index writing and checkout safety are the hard parts) | ✅ done |
| 4 Rewriting | 8 | ~3–4k LOC (sequencer and stash) | 🚧 5 of 8 |
| 5 MCP extras | resources, prompts, progress, multi-repo | ~1.5k LOC | planned |
| 6 Hardening | — | ongoing | planned |
