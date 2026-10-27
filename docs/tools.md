# Tool reference

gitrs-mcp exposes 25 read-only tools. This page covers each one: what it does, its arguments and defaults, the fields it returns, the errors it reports, and a real request and response.

- [Conventions](#conventions)
- [Shared types](#shared-types)
- Repository and refs: [`repo_info`](#repo_info), [`list_refs`](#list_refs), [`rev_parse`](#rev_parse), [`compare_refs`](#compare_refs), [`describe`](#describe), [`show_tag`](#show_tag), [`reflog`](#reflog), [`list_stashes`](#list_stashes)
- Local changes: [`status`](#status), [`diff`](#diff)
- Files: [`read_file`](#read_file), [`list_tree`](#list_tree), [`blame`](#blame), [`grep`](#grep), [`check_ignore`](#check_ignore), [`check_attr`](#check_attr)
- History: [`log`](#log), [`show_commit`](#show_commit), [`file_history`](#file_history), [`search_commits`](#search_commits), [`notes`](#notes)
- Repository setup: [`list_worktrees`](#list_worktrees), [`list_submodules`](#list_submodules), [`config_get`](#config_get), [`count_objects`](#count_objects)

## Conventions

These rules hold for every tool.

- **Revisions** accept anything `git rev-parse` does: full or abbreviated SHAs, branch and tag names, `HEAD~2`, `v1.0^{tree}`, `main@{1}`, `:/message text`. Annotated tags are peeled to the commit they point at when a commit is needed.
- **Paths and pathspecs** are relative to the repository root, whatever directory the server was started from. Paths may not be absolute or contain `..`. Pathspec arguments accept git's pathspec syntax, including globs (`*.rs`) and magic such as `:(exclude)docs`.
- **Lists are paginated.** List tools take `limit` (and usually `skip`) and return `truncated: true` when more items exist. The default and maximum `limit` differ per tool and are listed below.
- **Large content has a byte budget.** `read_file`, `notes` and `diff` with `format: "patch"` return at most `max_bytes` (default 65536, at most 1048576) and say when they stopped early. `grep` cuts each matching line at 512 bytes.
- **Errors are tool errors.** Bad input, such as an unknown revision, a missing path or an invalid range, comes back as a tool result with `isError: true`. The message says what was wrong and how to fix it, for example:

  ```
  `src/nope.rs` does not exist at `HEAD`; check the path with list_tree
  ```

- **Output** is JSON in `structuredContent`, described by each tool's published `outputSchema`. The same JSON is also sent as text content for clients that don't read structured output. SHAs are always full 40-character hex unless a field is named `short_id`.

The examples come from a small demo repository, which [`scripts/demo-repo.sh`](../scripts/demo-repo.sh) builds with fixed dates so that its SHAs are reproducible: `main` has three commits (`Initial commit`, `Add shout`, `Document usage`) and an annotated tag `v0.1.0` on `Add shout`; `feature` branches from `Add shout` with one commit, `Add farewell`. `Document usage` has a note, and there is one stash. `vendor/assets` is a submodule, `origin` is a remote whose URL contains a password (never fetched), and `demo-review` is a locked linked worktree on `feature`. The working tree has an unstaged edit to `src/lib.rs`, a staged new file `src/main.rs`, and an untracked `notes.txt`. Each example shows the call's `arguments` and the `structuredContent` it returned. Local paths are shortened to `/path/to`.

## Shared types

**Signature**: a person and a timestamp, as recorded in a commit.

| Field | Type | Description |
|---|---|---|
| `name` | string | Name as recorded in the commit. |
| `email` | string | Email as recorded in the commit. |
| `time` | integer | Seconds since the Unix epoch. |
| `offset` | integer | UTC offset in seconds. |

**LogEntry**: one commit in a history listing.

| Field | Type | Description |
|---|---|---|
| `id` | string | Full commit SHA. |
| `short_id` | string | Abbreviated SHA, unique within the repository. |
| `summary` | string | First line of the commit message. |
| `author` | Signature | Who wrote the change, and when. |
| `parents` | string[] | Full SHAs of the parent commits; empty for a root commit. |

**ChangeKind**: how a path changed. One of `added`, `modified`, `deleted`, `renamed`, `copied`, `typechange`. A `typechange` is a switch between regular file, symlink and submodule. Flipping only the executable bit counts as `modified`.

---

## `repo_info`

Repository overview: paths, HEAD, current branch, bareness, shallowness, and any operation in progress.

**Arguments:** none.

**Result**

| Field | Type | Description |
|---|---|---|
| `git_dir` | string | Absolute path of the `.git` directory (the repository itself when bare). |
| `workdir` | string \| null | Working tree root; `null` for bare repositories. |
| `is_bare` | boolean | True for a repository without a working tree. |
| `is_shallow` | boolean | True for a shallow clone, whose history is cut off. |
| `head_branch` | string \| null | Short branch name HEAD points to; `null` when detached. |
| `head_commit` | string \| null | Commit HEAD resolves to; `null` in a fresh repository with no commits. |
| `in_progress` | string \| null | In-progress operation such as `Merge` or `Rebase`, if any. |

**Example**

```json
{}
```

```json
{
  "git_dir": "/path/to/demo/.git",
  "head_branch": "main",
  "head_commit": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
  "in_progress": null,
  "is_bare": false,
  "is_shallow": false,
  "workdir": "/path/to/demo"
}
```

## `list_refs`

List branches and/or tags with the commit each points to.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `kind` | `branches` \| `tags` \| `all` | `branches` | Which refs to list. |

**Result**

| Field | Type | Description |
|---|---|---|
| `refs[]` | array | Matching refs, sorted by full name. |
| `refs[].name` | string | Full name, e.g. `refs/heads/main`. |
| `refs[].short_name` | string | Short name, e.g. `main`. |
| `refs[].target` | string \| null | Commit the ref ultimately points to (annotated tags are peeled). |
| `refs[].is_head` | boolean | True for the branch HEAD is on. |

**Example**

```json
{"kind": "all"}
```

```json
{
  "refs": [
    {"is_head": false, "name": "refs/heads/feature", "short_name": "feature", "target": "e9383dc8fce1955c63812c2365edbff4efdffce4"},
    {"is_head": true, "name": "refs/heads/main", "short_name": "main", "target": "48ce6b19c89fc67695e4e723b8192fb167fb8f00"},
    {"is_head": false, "name": "refs/notes/commits", "short_name": "notes/commits", "target": "d88da5dd41aab8e6ed9a334203b8caff338e3549"},
    {"is_head": false, "name": "refs/stash", "short_name": "refs/stash", "target": "dbcd799721ed9636188e6497d1bad21738471383"},
    {"is_head": false, "name": "refs/tags/v0.1.0", "short_name": "v0.1.0", "target": "41dad034dd469fe144a58a71bcd86453cd70ade1"}
  ]
}
```

## `rev_parse`

Resolve a revision expression to full object SHAs, like `git rev-parse`. Use it to check that a name exists, or to pin a moving ref such as a branch to a SHA before later calls.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `spec` | string | required | A revision or range: a SHA or prefix, a ref name, `HEAD~2`, `v1.0^{tree}`, `main:src/lib.rs`, `:/fix typo`, `main..feature`, `main...feature`, `^main`, `HEAD^@`, `HEAD^!`. |

**Result**

| Field | Type | Description |
|---|---|---|
| `kind` | string | The shape of the spec, see below. |
| `ids` | string[] | Full SHAs the spec resolved to, in the order they appear in the spec. |
| `object_type` | `commit` \| `tree` \| `blob` \| `tag` | Type of the first object in `ids`; range endpoints are commits. |
| `merge_base` | string \| null | For `merge_base_range`: the best common ancestor of both sides. `null` for other kinds, or when the histories share no commit. |

`kind` is one of:

| Kind | Spec | `ids` |
|---|---|---|
| `single` | `HEAD~2`, `main:src/lib.rs` | the object |
| `exclude` | `^rev` | `rev` |
| `range` | `a..b` | `[a, b]` |
| `merge_base_range` | `a...b` | `[a, b]` |
| `include_only_parents` | `rev^@` | `rev` |
| `exclude_parents` | `rev^!` | `rev` |

**Errors:** an unknown revision: `` cannot resolve `no-such-branch`; pass a SHA, ref name, an expression such as HEAD~2, or a range such as main..feature ``.

**Examples**

```json
{"spec": "main~1"}
```

```json
{"ids": ["41dad034dd469fe144a58a71bcd86453cd70ade1"], "kind": "single", "merge_base": null, "object_type": "commit"}
```

```json
{"spec": "main...feature"}
```

```json
{
  "ids": ["48ce6b19c89fc67695e4e723b8192fb167fb8f00", "e9383dc8fce1955c63812c2365edbff4efdffce4"],
  "kind": "merge_base_range",
  "merge_base": "41dad034dd469fe144a58a71bcd86453cd70ade1",
  "object_type": "commit"
}
```

## `compare_refs`

How two revisions have diverged: their merge base, how many commits `head` is ahead of and behind `base`, and the newest of those commits. Use it before merging, or to check whether a branch is up to date.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `base` | string | required | The reference point, usually the branch you'd merge into (e.g. `main`). Any revision works. |
| `head` | string | required | The revision to compare against `base` (e.g. `feature` or `HEAD`). |
| `limit` | integer | 20 (max 500) | Maximum commits listed in each of `ahead_commits` and `behind_commits`. The counts are always complete. |

**Result**

| Field | Type | Description |
|---|---|---|
| `base`, `head` | string | Full SHAs the arguments resolved to. |
| `merge_base` | string \| null | Best common ancestor; `null` when the histories share no commit. |
| `ahead` | integer | Commits in `head` that are not in `base` (`git rev-list --count base..head`). |
| `behind` | integer | Commits in `base` that are not in `head` (`git rev-list --count head..base`). |
| `ahead_commits` | LogEntry[] | The newest `ahead` commits, newest first. |
| `behind_commits` | LogEntry[] | The newest `behind` commits, newest first. |
| `truncated` | boolean | True if either list was cut off at `limit`. |

**Errors:** either revision can't be resolved to a commit.

**Example**

```json
{"base": "main", "head": "feature"}
```

```json
{
  "ahead": 1,
  "ahead_commits": [
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791036000},
      "id": "e9383dc8fce1955c63812c2365edbff4efdffce4",
      "parents": ["41dad034dd469fe144a58a71bcd86453cd70ade1"],
      "short_id": "e9383dc",
      "summary": "Add farewell"
    }
  ],
  "base": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
  "behind": 1,
  "behind_commits": [
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791101700},
      "id": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
      "parents": ["41dad034dd469fe144a58a71bcd86453cd70ade1"],
      "short_id": "48ce6b1",
      "summary": "Document usage"
    }
  ],
  "head": "e9383dc8fce1955c63812c2365edbff4efdffce4",
  "merge_base": "41dad034dd469fe144a58a71bcd86453cd70ade1",
  "truncated": false
}
```

---

## `describe`

Name a commit after the nearest tag in its history, like `git describe --always`. Use it to see which release a commit belongs to, or to label a build.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `rev` | string | `HEAD` | Commit to describe. |
| `tags` | boolean | `false` | Also consider lightweight tags, like `git describe --tags`. By default only annotated tags count. |
| `long` | boolean | `false` | Always use the long form `<tag>-<n>-g<sha>`, even when the commit is tagged, like `--long`. |
| `first_parent` | boolean | `false` | Follow only the first parent of merges, like `--first-parent`. |

**Result**

| Field | Type | Description |
|---|---|---|
| `description` | string | What `git describe --always` prints: `<tag>` on a tagged commit, `<tag>-<n>-g<short sha>` when the commit is `n` commits past the tag, or the short SHA alone when no tag is reachable. |
| `tag` | string \| null | Short name of the nearest reachable tag; `null` when none is reachable. |
| `depth` | integer | Commits in the described commit's history that aren't in the tag's; 0 without a tag. |
| `exact` | boolean | True when the commit itself carries `tag`. |
| `id` | string | Full SHA of the described commit. |

Finding no tag isn't an error: the result falls back to the short SHA with `tag: null`, as `git describe --always` does.

**Errors:** an unknown revision; a repository with no commits yet.

**Examples**

```json
{}
```

```json
{"depth": 1, "description": "v0.1.0-1-g48ce6b1", "exact": false, "id": "48ce6b19c89fc67695e4e723b8192fb167fb8f00", "tag": "v0.1.0"}
```

```json
{"rev": "HEAD~1", "long": true}
```

```json
{"depth": 0, "description": "v0.1.0-0-g41dad03", "exact": true, "id": "41dad034dd469fe144a58a71bcd86453cd70ade1", "tag": "v0.1.0"}
```

## `show_tag`

Details of one tag: whether it is annotated or lightweight, the object it points at and the commit it peels to, and for an annotated tag its tagger, date, message and whether it is signed.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `name` | string | required | Tag name, e.g. `v1.0` or `refs/tags/v1.0`. |

**Result**

| Field | Type | Description |
|---|---|---|
| `name` | string | Full ref name, e.g. `refs/tags/v1.0`. |
| `kind` | `annotated` \| `lightweight` | An annotated tag is a tag object with a tagger, date and message (`git tag -a`); a lightweight tag is a plain ref. |
| `tag_id` | string \| null | Full SHA of the tag object; `null` for a lightweight tag. |
| `target` | string | Full SHA the tag points at: the tag object's target, or the ref's target for a lightweight tag. |
| `target_type` | `commit` \| `tree` \| `blob` \| `tag` | Type of `target`; `tag` when an annotated tag points at another tag. |
| `peeled` | string | Full SHA after peeling every tag layer, usually a commit (`git rev-parse <tag>^{}`). |
| `peeled_type` | `commit` \| `tree` \| `blob` \| `tag` | Type of `peeled`. |
| `tagger` | Signature \| null | Who created the annotated tag, and when; `null` for a lightweight tag. |
| `message` | string \| null | The annotated tag's message, without any signature block; `null` for a lightweight tag. |
| `signed` | boolean | True when the annotated tag carries a PGP or SSH signature. The signature isn't verified. |

**Errors:** no such tag: `` no tag named `v9`; list_refs with kind `tags` shows the tags that exist ``.

**Example**

```json
{"name": "v0.1.0"}
```

```json
{
  "kind": "annotated",
  "message": "First release\n\ngreet and shout.\n",
  "name": "refs/tags/v0.1.0",
  "peeled": "41dad034dd469fe144a58a71bcd86453cd70ade1",
  "peeled_type": "commit",
  "signed": false,
  "tag_id": "76ad4a90a5f1ebaa255fcaddcb5014a2f4c10d62",
  "tagger": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1790937600},
  "target": "41dad034dd469fe144a58a71bcd86453cd70ade1",
  "target_type": "commit"
}
```

## `reflog`

Where a ref pointed over time, newest first, like `git reflog show <ref>`. Each entry is one update: the commit before and after, who made it, and the message git recorded (`commit: …`, `checkout: moving from main to feature`, `reset: moving to HEAD~1`, `rebase …`). Use it to find commits lost to a reset or rebase, or to see recent branch switches.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `ref` | string | `HEAD` | Ref whose reflog to read: `HEAD`, a branch such as `main`, or a full name such as `refs/heads/main`. Short names resolve like `git rev-parse`. |
| `limit` | integer | 20 (max 500) | Maximum entries to return. |
| `skip` | integer | 0 | Entries to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `ref` | string | Full name of the ref whose log was read, e.g. `refs/heads/main` or `HEAD`. |
| `entries[]` | array | Updates newest first. Empty when the ref has no reflog (tags normally don't). |
| `entries[].selector` | string | Revision naming the entry, such as `HEAD@{2}` or `main@{0}`. Pass it to other tools as `rev`. |
| `entries[].old_id` | string \| null | Full SHA before the update; `null` when the update created the ref. |
| `entries[].new_id` | string | Full SHA after the update. |
| `entries[].committer` | Signature | Who made the update, and when. |
| `entries[].message` | string | What git recorded for the update. |
| `truncated` | boolean | True if older entries exist past this page. |

**Errors:** an unknown ref: `` no ref named `nope`; pass HEAD, a branch name, or a full ref name (list_refs shows what exists) ``.

**Example**

```json
{"ref": "main", "limit": 2}
```

```json
{
  "entries": [
    {
      "committer": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791101700},
      "message": "commit: Document usage",
      "new_id": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
      "old_id": "41dad034dd469fe144a58a71bcd86453cd70ade1",
      "selector": "main@{0}"
    },
    {
      "committer": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1790937000},
      "message": "commit: Add shout",
      "new_id": "41dad034dd469fe144a58a71bcd86453cd70ade1",
      "old_id": "73656d92804c0ba5ffeb467a0ca8c8d7d7681bc3",
      "selector": "main@{1}"
    }
  ],
  "ref": "refs/heads/main",
  "truncated": true
}
```

## `list_stashes`

The stash list, newest first, like `git stash list`. git keeps stashes as the reflog of `refs/stash`, and this tool reads it; creating or applying stashes comes with the write tools.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `limit` | integer | 20 (max 500) | Maximum stashes to return. |
| `skip` | integer | 0 | Stashes to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `stashes[]` | array | Stashes newest first. Empty when there are none. |
| `stashes[].index` | integer | Position in the list; 0 is the most recent stash. |
| `stashes[].selector` | string | Revision naming the stash, e.g. `stash@{0}`. |
| `stashes[].id` | string | Full SHA of the stash commit, which records the stashed worktree state. |
| `stashes[].base` | string | Full SHA of the commit HEAD was on when the stash was made (the stash commit's first parent). |
| `stashes[].branch` | string \| null | Branch HEAD was on when stashing; `null` when HEAD was detached. |
| `stashes[].message` | string | The message `git stash list` shows, e.g. `WIP on main: 1a2b3c4 subject` or `On main: my message`. |
| `stashes[].includes_untracked` | boolean | True when the stash also holds untracked files (`git stash -u` or `-a`). |
| `stashes[].committer` | Signature | Who made the stash, and when. |
| `truncated` | boolean | True if older stashes exist past this page. |

To see what a stash holds, call `diff` with `from` set to its `base` and `to` set to its `selector`.

**Example**

```json
{}
```

```json
{
  "stashes": [
    {
      "base": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
      "branch": "main",
      "committer": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791105000},
      "id": "dbcd799721ed9636188e6497d1bad21738471383",
      "includes_untracked": false,
      "index": 0,
      "message": "On main: try a new intro",
      "selector": "stash@{0}"
    }
  ],
  "truncated": false
}
```

## `status`

Working-tree status like `git status`: the current branch, and staged, unstaged, untracked and conflicted paths. Start here to see what has changed locally.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `pathspec` | string[] | `[]` (everything) | Limit to these paths or pathspec patterns, e.g. `src/`, `*.rs`, `:(exclude)docs`. |
| `include_untracked` | boolean | `true` | List untracked files. Untracked directories are collapsed to `dir/`, like `git status`. |
| `include_ignored` | boolean | `false` | List ignored files in `ignored`. Ignored directories are collapsed to `dir/`. |
| `limit` | integer | 500 (max 5000) | Maximum entries in each list. `counts` always has the full totals. |

**Result**

| Field | Type | Description |
|---|---|---|
| `branch` | string \| null | Short name of the current branch; `null` when HEAD is detached. |
| `head` | string \| null | Commit HEAD points to; `null` when the branch has no commits yet. |
| `upstream` | string \| null | Upstream tracking ref of the current branch from local config (e.g. `refs/remotes/origin/main`), or `null` if none is configured. It is never fetched, so it may be stale. |
| `staged[]` | array | Changes between HEAD and the index: what `git commit` would record. |
| `unstaged[]` | array | Changes between the index and the working tree that are not staged yet. |
| `staged[].path`, `unstaged[].path` | string | Path relative to the repository root (the new path for renames). |
| `staged[].change`, `unstaged[].change` | ChangeKind | How the path changed. |
| `staged[].old_path` | string | Previous path for `renamed` and `copied`; omitted otherwise. |
| `untracked` | string[] | Untracked paths; directories end in `/`. |
| `ignored` | string[] | Ignored paths when `include_ignored` is set; directories end in `/`. |
| `conflicted[]` | array | Paths with unresolved merge conflicts, as `{path, conflict}`. These are not repeated in `staged` or `unstaged`. |
| `conflicted[].conflict` | string | `both_modified`, `both_added`, `both_deleted`, `added_by_us`, `added_by_them`, `deleted_by_us` or `deleted_by_them`. |
| `counts` | object | Size of each list (`staged`, `unstaged`, `untracked`, `ignored`, `conflicted`) before `limit` was applied. |
| `truncated` | boolean | True if any list was cut off at `limit`; narrow with `pathspec` to see the rest. |
| `clean` | boolean | True when nothing is staged, unstaged, conflicted or untracked. Ignored files don't count. |

Renames between HEAD and the index are detected like `git status`: `status.renames` decides, falling back to `diff.renames`, and is on by default. Setting either to `copies` also detects copies.

**Errors:** the repository is bare (there is no working tree).

**Example**

```json
{}
```

```json
{
  "branch": "main",
  "clean": false,
  "conflicted": [],
  "counts": {"conflicted": 0, "ignored": 0, "staged": 1, "unstaged": 1, "untracked": 1},
  "head": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
  "ignored": [],
  "staged": [{"change": "added", "path": "src/main.rs"}],
  "truncated": false,
  "unstaged": [{"change": "modified", "path": "src/lib.rs"}],
  "untracked": ["notes.txt"],
  "upstream": null
}
```

## `diff`

Changed files and lines, like `git diff`. Three comparisons are available:

| Arguments | Compares | Like |
|---|---|---|
| none | index → working tree (unstaged changes) | `git diff` |
| `staged: true` | `from` (default `HEAD`) → index (staged changes) | `git diff --cached [from]` |
| `from` + `to` | revision → revision | `git diff from to` |

Untracked files are never part of a diff; `status` lists them.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `from` | string | none | Old side: the first of two revisions with `to`, or the revision the index is compared with when `staged` is set. |
| `to` | string | none | New side when comparing two revisions; requires `from`. |
| `staged` | boolean | `false` | Compare the index with `from` (default `HEAD`). |
| `pathspec` | string[] | `[]` (everything) | Limit to these paths or pathspec patterns. |
| `format` | `stat` \| `name_status` \| `patch` | `stat` | `stat`: status plus line counts per file (`git diff --numstat`). `name_status`: status and paths only, fastest. `patch`: line counts plus a unified diff per file. |
| `context_lines` | integer | 3 | Unchanged lines of context around each change in patches. |
| `max_bytes` | integer | 65536 (max 1048576) | Byte budget for all patch text in the response. |
| `limit` | integer | 100 (max 1000) | Maximum files to return. `totals` always cover every file. |
| `skip` | integer | 0 | Files to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `mode` | `worktree` \| `staged` \| `revisions` | Which comparison was made. |
| `from` | string \| null | Full commit SHA of the old side; `null` when it is the index or not a commit (a tree SHA, or the empty tree of a repository without commits). |
| `to` | string \| null | Full commit SHA of the new side; `null` when it is the index or the working tree. |
| `files[]` | array | Changed files in path order. |
| `files[].path` | string | Path relative to the repository root; the new path for renames and copies. |
| `files[].old_path` | string | Previous path for `renamed` and `copied`; omitted otherwise. |
| `files[].status` | ChangeKind | How the file changed. |
| `files[].additions`, `files[].deletions` | integer \| null | Lines added and deleted; `null` for binary files and with `name_status`. |
| `files[].binary` | boolean | True if either side is binary. Binary files get no line counts and only a one-line patch. |
| `files[].patch` | string \| null | Unified diff starting at `diff --git`, ready for `git apply`. Only with format `patch`; `null` when the byte budget ran out before this file. |
| `files[].patch_truncated` | boolean | True if `patch` was cut at the byte budget and won't apply as-is. |
| `totals` | object | `files`, `additions` and `deletions` summed over every changed file, not just this page. Line totals are `null` with `name_status`. |
| `unmerged` | string[] | Paths with unresolved merge conflicts. They aren't diffed; `status` reports their conflict kind. |
| `truncated` | boolean | True if `files` was cut off at `limit`, or a patch was dropped or cut for the byte budget. |

Patches follow git's format exactly: `index` lines, mode changes, `rename from`/`copy from` headers, `\ No newline at end of file` markers, and hunk headers with git's default function context. Submodules diff as `Subproject commit <sha>` lines, and only when they point at a different commit; edits inside a submodule aren't shown, as with `git diff --ignore-submodules=dirty`.

When patches don't fit in `max_bytes`, the first patch of a response is cut at a line boundary (`patch_truncated: true`) and later ones are left out (`patch: null`). Fetch the rest by calling again with `skip`, or with a larger `max_bytes`.

**Errors:** `to` without `from`; `to` together with `staged`; `from` alone (comparing a revision with the working tree isn't supported); an unknown revision; a bare repository in the `worktree` or `staged` modes.

**Examples**

```json
{"format": "patch"}
```

```json
{
  "files": [
    {
      "additions": 1,
      "binary": false,
      "deletions": 1,
      "patch": "diff --git a/src/lib.rs b/src/lib.rs\nindex b1a7078..de41029 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,5 +1,5 @@\n pub fn greet(name: &str) -> String {\n-    format!(\"Hello, {name}!\")\n+    format!(\"Hello, {name}!!\")\n }\n \n pub fn shout(name: &str) -> String {\n",
      "patch_truncated": false,
      "path": "src/lib.rs",
      "status": "modified"
    }
  ],
  "from": null,
  "mode": "worktree",
  "to": null,
  "totals": {"additions": 1, "deletions": 1, "files": 1},
  "truncated": false,
  "unmerged": []
}
```

```json
{"staged": true, "format": "name_status"}
```

```json
{
  "files": [
    {
      "additions": null,
      "binary": false,
      "deletions": null,
      "patch": null,
      "patch_truncated": false,
      "path": "src/main.rs",
      "status": "added"
    }
  ],
  "from": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
  "mode": "staged",
  "to": null,
  "totals": {"additions": null, "deletions": null, "files": 1},
  "truncated": false,
  "unmerged": []
}
```

An unsupported combination:

```json
{"from": "main"}
```

```
comparing a revision with the working tree isn't supported; pass `to` for two revisions, or set `staged: true` to compare `from` with the index
```

---

## `read_file`

Read a file at a revision, from the index, or from the working tree. Text comes back as UTF-8 and binary content as base64. Large files are read in chunks.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `path` | string | required | File path relative to the repository root. |
| `rev` | string | `HEAD` | A revision, or `index` for the staged version, or `worktree` for the file on disk. A branch literally named `index` or `worktree` can be read as `refs/heads/index`. |
| `offset_bytes` | integer | 0 | Byte offset to start at. Pass `next_offset` from a truncated result to continue. |
| `max_bytes` | integer | 65536 (max 1048576) | Maximum bytes of content to return. |

**Result**

| Field | Type | Description |
|---|---|---|
| `path` | string | The normalized path that was read. |
| `content` | string \| null | The requested range as UTF-8 text; `null` when `binary` is true. |
| `base64` | string \| null | The requested range as standard base64; `null` unless `binary` is true. |
| `binary` | boolean | True when the file has a NUL byte in its first 8000 bytes (git's rule) or the range isn't valid UTF-8. |
| `size` | integer | Total size of the file in bytes. |
| `mode` | string | Git file mode in octal: `100644` regular, `100755` executable, `120000` symlink. A symlink's content is its target. |
| `blob_id` | string \| null | SHA of the blob; `null` when reading from the worktree. |
| `truncated` | boolean | True if the returned range ends before the end of the file. |
| `next_offset` | integer \| null | Offset to pass as `offset_bytes` for the next chunk; `null` once the end is reached. |

A chunk never ends in the middle of a UTF-8 character, so `next_offset` may be a few bytes short of `offset_bytes + max_bytes`.

**Errors:** the path doesn't exist at the revision, or is a directory (use `list_tree`) or a submodule; the path is untracked or conflicted when reading `index`; `offset_bytes` is past the end of the file. Worktree reads also refuse paths inside `.git/`, and paths that lead outside the repository through a symlinked directory. A symlink itself is reported by its target and never followed.

**Example**

```json
{"path": "src/lib.rs", "rev": "v0.1.0", "max_bytes": 60}
```

```json
{
  "base64": null,
  "binary": false,
  "blob_id": "b1a70783e894333fced69f9441659e12c08cd16e",
  "content": "pub fn greet(name: &str) -> String {\n    format!(\"Hello, {na",
  "mode": "100644",
  "next_offset": 60,
  "path": "src/lib.rs",
  "size": 140,
  "truncated": true
}
```

## `list_tree`

List the files and directories of a revision's tree, like `git ls-tree`.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `rev` | string | `HEAD` | Any commit-ish, or a tree SHA. |
| `path` | string | the root | Directory to list. A file path lists just that entry. |
| `recursive` | boolean | `false` | List everything below `path` instead of only its direct children. Like `git ls-tree -r`, recursive listings contain files, symlinks and submodules but not directories. |
| `limit` | integer | 500 (max 5000) | Maximum entries to return. |
| `skip` | integer | 0 | Entries to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `entries[]` | array | Entries in git's tree order, the same order as `git ls-tree`. |
| `entries[].path` | string | Path relative to the repository root. |
| `entries[].kind` | `blob` \| `tree` \| `commit` \| `symlink` | File, directory, submodule (`commit`) or symlink. |
| `entries[].mode` | string | Git file mode in octal: `100644` file, `100755` executable, `120000` symlink, `040000` directory, `160000` submodule. |
| `entries[].id` | string | Full SHA of the object; for a submodule, the commit it records. |
| `entries[].size` | integer \| null | Size in bytes for files and symlinks; `null` for directories and submodules. |
| `total` | integer | Entries in the full listing, before pagination. |
| `truncated` | boolean | True if more entries exist past this page. |

**Errors:** an unknown revision; `path` doesn't exist at the revision.

**Example**

```json
{"path": "src", "rev": "feature", "recursive": true}
```

```json
{
  "entries": [
    {"id": "2b0aa2832e52266607f6638bf8398ab340432063", "kind": "blob", "mode": "100644", "path": "src/farewell.rs", "size": 74},
    {"id": "b1a70783e894333fced69f9441659e12c08cd16e", "kind": "blob", "mode": "100644", "path": "src/lib.rs", "size": 140}
  ],
  "total": 2,
  "truncated": false
}
```

## `blame`

Which commit last changed each line of a file at a revision, following renames like `git blame`. Consecutive lines from the same commit form one hunk.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `path` | string | required | File to blame, relative to the repository root. |
| `rev` | string | `HEAD` | Revision to blame at. Uncommitted changes are not blamed. |
| `line_range` | `[start, end]` | whole file | Only blame these lines: 1-based and inclusive, e.g. `[10, 20]`. |
| `limit` | integer | 200 (max 2000) | Maximum hunks to return. To continue, call again with `line_range` starting after the last returned hunk. |

**Result**

| Field | Type | Description |
|---|---|---|
| `path` | string | The normalized path that was blamed. |
| `rev` | string | Full SHA of the commit the blame started from. |
| `line_count` | integer | Number of lines in the file at `rev`. |
| `hunks[]` | array | Hunks in file order. Adjacent hunks can share a commit when its lines came from different places. |
| `hunks[].start_line` | integer | First line of the hunk in the file at `rev` (1-based). |
| `hunks[].lines` | integer | Number of consecutive lines in the hunk. |
| `hunks[].commit_id` | string | Full SHA of the commit that last changed these lines. |
| `hunks[].author` | Signature | Author of that commit. |
| `hunks[].summary` | string | First line of that commit's message. |
| `hunks[].original_path` | string | Path of the file in that commit; differs from `path` if the file was renamed since. |
| `hunks[].original_start_line` | integer | Line in `original_path` at that commit where the hunk starts (1-based). |
| `truncated` | boolean | True if hunks were cut off at `limit`. |

**Errors:** the path doesn't exist at the revision or isn't a file; `line_range` is outside the file (the message gives the line count).

**Example**

```json
{"path": "src/lib.rs", "line_range": [1, 3]}
```

```json
{
  "hunks": [
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1790845200},
      "commit_id": "73656d92804c0ba5ffeb467a0ca8c8d7d7681bc3",
      "lines": 3,
      "original_path": "src/lib.rs",
      "original_start_line": 1,
      "start_line": 1,
      "summary": "Initial commit"
    }
  ],
  "line_count": 7,
  "path": "src/lib.rs",
  "rev": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
  "truncated": false
}
```

---

## `grep`

Search file contents for a regular expression, like `git grep`. By default it searches tracked files as they are in the working tree, including unstaged edits; it can also search the index or any revision. Results are ordered by path, then line.

Like `git grep -I`, it skips binary files (a NUL byte in the first 8000 bytes), symlinks, submodules and untracked files, and in the working tree it skips tracked files that were deleted.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `pattern` | string | required | Regular expression matched against each line, in Rust [`regex`](https://docs.rs/regex/latest/regex/#syntax) syntax: Perl-like classes and flags (`\d`, `\b`, `(?i)`, `a\|b`), but no backreferences or lookaround. |
| `rev` | string | `worktree` | What to search: `worktree` for tracked files on disk (`git grep`), `index` for staged content (`git grep --cached`), or a revision such as `HEAD` or `v1.0` (`git grep <pattern> <rev>`). A bare repository defaults to `HEAD`. |
| `pathspec` | string[] | `[]` (everything) | Limit to these paths or pathspec patterns, e.g. `src/`, `*.rs`, `:(exclude)tests`. |
| `ignore_case` | boolean | `false` | Match regardless of case (`-i`). |
| `fixed_strings` | boolean | `false` | Treat `pattern` as literal text, not a regex (`-F`). |
| `limit` | integer | 100 (max 1000) | Maximum matching lines to return. |
| `skip` | integer | 0 | Matching lines to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `rev` | string \| null | Full SHA of the commit or tree that was searched; `null` for the worktree or index. |
| `matches[]` | array | Matching lines, ordered by path and then line number. |
| `matches[].path` | string | Path relative to the repository root. |
| `matches[].line` | integer | 1-based line number. |
| `matches[].column` | integer | 1-based byte offset where the first match on the line starts, like `git grep --column`. |
| `matches[].text` | string | The line without its terminator, cut at 512 bytes. Invalid UTF-8 is replaced with U+FFFD. |
| `matches[].text_truncated` | boolean | True if `text` was cut at 512 bytes. |
| `truncated` | boolean | True if more matches exist past this page; continue with `skip`. |

**Errors:** an invalid regex (the message suggests `fixed_strings`); an unknown revision, or one that isn't a commit or tree; `worktree` or `index` in a bare repository.

**Examples**

```json
{"pattern": "greet"}
```

```json
{
  "matches": [
    {"column": 3, "line": 1, "path": "README.md", "text": "# greet", "text_truncated": false},
    {"column": 8, "line": 3, "path": "README.md", "text": "A tiny greeting library.", "text_truncated": false},
    {"column": 10, "line": 8, "path": "README.md", "text": "let hi = greet::greet(\"Ada\");", "text_truncated": false},
    {"column": 8, "line": 1, "path": "src/lib.rs", "text": "pub fn greet(name: &str) -> String {", "text_truncated": false},
    {"column": 5, "line": 6, "path": "src/lib.rs", "text": "    greet(name).to_uppercase()", "text_truncated": false},
    {"column": 20, "line": 2, "path": "src/main.rs", "text": "    println!(\"{}\", greet::greet(\"world\"));", "text_truncated": false}
  ],
  "rev": null,
  "truncated": false
}
```

```json
{"pattern": "fn \\w+\\(name", "rev": "feature", "pathspec": ["src/"], "limit": 2}
```

```json
{
  "matches": [
    {"column": 5, "line": 1, "path": "src/farewell.rs", "text": "pub fn farewell(name: &str) -> String {", "text_truncated": false},
    {"column": 5, "line": 1, "path": "src/lib.rs", "text": "pub fn greet(name: &str) -> String {", "text_truncated": false}
  ],
  "rev": "e9383dc8fce1955c63812c2365edbff4efdffce4",
  "truncated": true
}
```

An invalid regex:

```json
{"pattern": "greet(", "rev": "v0.1.0"}
```

```
`pattern` isn't a valid regex; fix it, or set `fixed_strings: true` to search for `greet(` literally: regex parse error:
    greet(
         ^
error: unclosed group
```

## `check_ignore`

Whether paths are ignored, and by which rule, like `git check-ignore -v --non-matching`. Rules come from `.gitignore` files in the worktree (or the index), `.git/info/exclude`, and `core.excludesFile`. The paths don't have to exist.

As in git, a path inside an ignored directory is ignored by that directory's rule, and no rule further down can re-include it. Tracked files are never ignored, whatever the rules say; `pattern` still shows the rule that would apply (`git check-ignore --no-index`).

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `paths` | string[] | required | Paths relative to the repository root, at most 1000. A path that exists as a directory, or ends with `/`, is checked as a directory. |

**Result**

| Field | Type | Description |
|---|---|---|
| `paths[]` | array | One answer per requested path, in request order. |
| `paths[].path` | string | The path as checked (a trailing `/` is kept). |
| `paths[].ignored` | boolean | True when git ignores the path: a rule matches it or a parent directory, the rule isn't a `!` negation, and the path isn't tracked. |
| `paths[].tracked` | boolean | True when the path is in the index. |
| `paths[].pattern` | string \| null | The deciding rule as written, e.g. `*.log`, `target/` or `!keep.log`; `null` when no rule matches. |
| `paths[].source` | string \| null | File the rule comes from, relative to the repository root when inside it (`.gitignore`, `src/.gitignore`, `.git/info/exclude`), otherwise absolute. |
| `paths[].line` | integer \| null | 1-based line of the rule in `source`. |

**Errors:** an empty `paths`, more than 1000 paths, or a path that is absolute or contains `..`.

**Example**

```json
{"paths": ["target/debug/app", "server.log", "src/lib.rs", "notes.txt"]}
```

```json
{
  "paths": [
    {"ignored": true, "line": 1, "path": "target/debug/app", "pattern": "target/", "source": ".gitignore", "tracked": false},
    {"ignored": true, "line": 2, "path": "server.log", "pattern": "*.log", "source": ".gitignore", "tracked": false},
    {"ignored": false, "line": null, "path": "src/lib.rs", "pattern": null, "source": null, "tracked": true},
    {"ignored": false, "line": null, "path": "notes.txt", "pattern": null, "source": null, "tracked": false}
  ]
}
```

## `check_attr`

Git attributes of paths, like `git check-attr`. Attributes come from `.gitattributes` files, `.git/info/attributes` and `core.attributesFile`, and decide how git treats a file: line endings (`text`, `eol`), `binary`, diff and merge drivers, `export-ignore`, `linguist-*` and so on. The paths don't have to exist.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `paths` | string[] | required | Paths relative to the repository root, at most 1000. |
| `attrs` | string[] | `[]` | Attribute names to look up, e.g. `["text", "diff"]`. Empty means every attribute set on each path, like `git check-attr -a`. |

**Result**

| Field | Type | Description |
|---|---|---|
| `paths[]` | array | One answer per requested path, in request order. |
| `paths[].path` | string | The path as checked. |
| `paths[].attributes[]` | array | The requested attributes in request order; with `attrs` empty, every attribute that is set, unset or has a value. Macros expand, so `binary` also reports `diff`, `merge` and `text` as unset. With `attrs` empty the order can differ from git's, which lists macros first. |
| `paths[].attributes[].name` | string | Attribute name. |
| `paths[].attributes[].state` | `set` \| `unset` \| `value` \| `unspecified` | `set` (`text`), `unset` (`-text`), `value` (`diff=rust`), or `unspecified` when no rule mentions it; `unspecified` only appears for attributes named in `attrs`. |
| `paths[].attributes[].value` | string \| null | The value when `state` is `value`. |

In a bare repository only `.git/info/attributes` and `core.attributesFile` apply, because there is no worktree or index to read `.gitattributes` from.

**Errors:** an empty `paths`, more than 1000 paths, or a path that is absolute or contains `..`.

**Examples**

```json
{"paths": ["src/lib.rs", "logo.png"]}
```

```json
{
  "paths": [
    {"attributes": [{"name": "diff", "state": "value", "value": "rust"}], "path": "src/lib.rs"},
    {
      "attributes": [
        {"name": "binary", "state": "set", "value": null},
        {"name": "diff", "state": "unset", "value": null},
        {"name": "merge", "state": "unset", "value": null},
        {"name": "text", "state": "unset", "value": null}
      ],
      "path": "logo.png"
    }
  ]
}
```

```json
{"paths": ["src/lib.rs", "README.md"], "attrs": ["diff", "text"]}
```

```json
{
  "paths": [
    {
      "attributes": [{"name": "diff", "state": "value", "value": "rust"}, {"name": "text", "state": "unspecified", "value": null}],
      "path": "src/lib.rs"
    },
    {
      "attributes": [{"name": "diff", "state": "unspecified", "value": null}, {"name": "text", "state": "unspecified", "value": null}],
      "path": "README.md"
    }
  ]
}
```

## `log`

Commit history starting from a revision, newest first.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `rev` | string | `HEAD` | Revision to start from. |
| `limit` | integer | 20 (max 500) | Maximum commits to return. |
| `skip` | integer | 0 | Commits to skip first, for pagination. |
| `first_parent` | boolean | `false` | Follow only the first parent of merges. |

**Result**

| Field | Type | Description |
|---|---|---|
| `commits` | LogEntry[] | Commits newest first. |
| `truncated` | boolean | True if more commits exist past this page. |

**Errors:** an unknown revision; a repository with no commits yet.

**Example**

```json
{"limit": 2}
```

```json
{
  "commits": [
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791101700},
      "id": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
      "parents": ["41dad034dd469fe144a58a71bcd86453cd70ade1"],
      "short_id": "48ce6b1",
      "summary": "Document usage"
    },
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1790937000},
      "id": "41dad034dd469fe144a58a71bcd86453cd70ade1",
      "parents": ["73656d92804c0ba5ffeb467a0ca8c8d7d7681bc3"],
      "short_id": "41dad03",
      "summary": "Add shout"
    }
  ],
  "truncated": true
}
```

## `show_commit`

Full metadata and message of one commit.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `rev` | string | `HEAD` | Commit to show. |

**Result**

| Field | Type | Description |
|---|---|---|
| `id` | string | Full commit SHA. |
| `tree` | string | Full SHA of the commit's root tree. |
| `parents` | string[] | Full SHAs of the parent commits; empty for a root commit. |
| `author` | Signature | Who wrote the change, and when. |
| `committer` | Signature | Who created the commit, and when; differs from `author` after rebases and cherry-picks. |
| `summary` | string | First line of the commit message. |
| `message` | string | Full raw message including the summary line. |

**Example**

```json
{"rev": "main"}
```

```json
{
  "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791101700},
  "committer": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791101700},
  "id": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
  "message": "Document usage\n\nFixes #12\n",
  "parents": ["41dad034dd469fe144a58a71bcd86453cd70ade1"],
  "summary": "Document usage",
  "tree": "12304c5496bbf08bd3066544fe1cd1932f3f392a"
}
```

## `file_history`

Commits that changed a file or directory, newest first, with how each commit changed it. The list matches `git log -- <path>`, including git's default history simplification: a merge that took the path unchanged from one parent is hidden, and only that parent's history is followed.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `path` | string | required | File or directory to trace, relative to the repository root. |
| `rev` | string | `HEAD` | Revision to start from. |
| `limit` | integer | 20 (max 500) | Maximum commits to return. |
| `skip` | integer | 0 | Matching commits to skip first, for pagination. |
| `follow_renames` | boolean | `false` | Keep following a single file across renames, like `git log --follow`. |

**Result**

| Field | Type | Description |
|---|---|---|
| `commits[]` | array | LogEntry fields (`id`, `short_id`, `summary`, `author`, `parents`) plus the fields below. |
| `commits[].change` | ChangeKind | How the path changed in this commit, compared with its first parent. |
| `commits[].path` | string | The path as it was in this commit. It differs from the requested path once a rename was followed. |
| `commits[].old_path` | string | The path before this commit, for `renamed`; omitted otherwise. |
| `truncated` | boolean | True if more matching commits exist past this page. |

A path that never existed returns an empty list, not an error.

**Errors:** `follow_renames` on a directory; an unknown revision; an absolute path or one containing `..`.

**Example**

```json
{"path": "src/lib.rs"}
```

```json
{
  "commits": [
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1790937000},
      "change": "modified",
      "id": "41dad034dd469fe144a58a71bcd86453cd70ade1",
      "parents": ["73656d92804c0ba5ffeb467a0ca8c8d7d7681bc3"],
      "path": "src/lib.rs",
      "short_id": "41dad03",
      "summary": "Add shout"
    },
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1790845200},
      "change": "added",
      "id": "73656d92804c0ba5ffeb467a0ca8c8d7d7681bc3",
      "parents": [],
      "path": "src/lib.rs",
      "short_id": "73656d9",
      "summary": "Initial commit"
    }
  ],
  "truncated": false
}
```

## `search_commits`

Find commits by message, author, date and changed path. Every filter you pass must match; with no filters it lists all commits like `log`.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `message_regex` | string | none | Regular expression matched anywhere in the full commit message. Rust `regex` syntax, case-sensitive; start with `(?i)` to ignore case. |
| `author` | string | none | Regular expression matched against the author as `Name <email>`, like `git log --author`. |
| `since` | string | none | Only commits whose committer date is at or after this time. Accepts ISO 8601 (`2024-01-31`, `2024-01-31T12:00:00Z`), RFC 2822, Unix seconds, or relative dates (`2 weeks ago`, `yesterday`). |
| `until` | string | none | Only commits whose committer date is at or before this time. Same formats as `since`. |
| `path` | string | none | Only commits that changed this file or directory, with the same rules as `file_history`. |
| `rev` | string | `HEAD` | Revision to search from. |
| `limit` | integer | 20 (max 500) | Maximum commits to return. |
| `skip` | integer | 0 | Matching commits to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `commits` | LogEntry[] | Matching commits, newest first by committer date. |
| `truncated` | boolean | True if more matching commits exist past this page. |

**Errors:** an invalid regular expression (the message names the field); a date that can't be parsed; an unknown revision.

**Example**

```json
{"message_regex": "(?i)fixes #\\d+"}
```

```json
{
  "commits": [
    {
      "author": {"email": "ada@example.com", "name": "Ada Lovelace", "offset": 0, "time": 1791101700},
      "id": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
      "parents": ["41dad034dd469fe144a58a71bcd86453cd70ade1"],
      "short_id": "48ce6b1",
      "summary": "Document usage"
    }
  ],
  "truncated": false
}
```

## `notes`

Notes attached to a commit or another object, like `git notes show`. Notes are text that tools and reviewers add to commits after the fact (for example review sign-offs or CI results) without changing the commit.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `rev` | string | `HEAD` | Object whose notes to show. Usually a commit; tags aren't peeled, so an annotated tag's notes are the ones on the tag object, as in `git notes show`. |
| `ref` | string | see description | Notes ref to read, e.g. `refs/notes/review`, or `review`, which expands to `refs/notes/review`. By default, `core.notesRef` (normally `refs/notes/commits`) plus any `notes.displayRef` refs. |
| `max_bytes` | integer | 65536 (max 1048576) | Maximum bytes of text returned per note. |

**Result**

| Field | Type | Description |
|---|---|---|
| `id` | string | Full SHA of the object the notes are attached to. |
| `notes[]` | array | One note per notes ref that annotates the object, in display order. Empty when there are none, including when the notes ref doesn't exist. |
| `notes[].ref` | string | Notes ref the note comes from. |
| `notes[].note_id` | string | Full SHA of the note's blob. |
| `notes[].text` | string | The note text. Invalid UTF-8 is replaced with U+FFFD. |
| `notes[].truncated` | boolean | True if `text` was cut at `max_bytes`. |

**Errors:** an unknown revision; a repository with no commits yet.

**Example**

```json
{}
```

```json
{
  "id": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
  "notes": [
    {
      "note_id": "6127b25d6086a22684f64503d9e7f8f921f4e7fb",
      "ref": "refs/notes/commits",
      "text": "Reviewed-by: Grace Hopper <grace@example.com>\n",
      "truncated": false
    }
  ]
}
```

## `list_worktrees`

Every worktree of the repository, like `git worktree list`: the main worktree first, then linked worktrees (created with `git worktree add`) ordered by their id.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `limit` | integer | 100 (max 1000) | Maximum worktrees to return. |
| `skip` | integer | 0 | Worktrees to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `worktrees[]` | array | The main worktree first, then linked worktrees ordered by `id`. |
| `worktrees[].path` | string \| null | Absolute path of the checkout (for a bare main repository, the repository itself); `null` when a linked worktree's registration can't be read. |
| `worktrees[].id` | string \| null | Name of a linked worktree, its directory under `.git/worktrees/`; `null` for the main worktree. |
| `worktrees[].is_main` | boolean | True for the main worktree. |
| `worktrees[].is_bare` | boolean | True when the main repository is bare. `git worktree list` prints no HEAD or branch for it; this tool still reports them. |
| `worktrees[].is_current` | boolean | True for the worktree this server serves. |
| `worktrees[].head` | string \| null | Full SHA of the worktree's HEAD; `null` when its branch has no commits yet. |
| `worktrees[].branch` | string \| null | Short name of the branch checked out; `null` when HEAD is detached. |
| `worktrees[].detached` | boolean | True when HEAD is detached. |
| `worktrees[].locked` | boolean | True when locked with `git worktree lock`, so it won't be pruned, moved or removed. |
| `worktrees[].lock_reason` | string \| null | The reason given when locking. |
| `worktrees[].prunable` | boolean | True when the checkout directory is gone, so `git worktree prune` would remove the registration. |
| `truncated` | boolean | True if more worktrees exist past this page. |

**Example**

```json
{}
```

```json
{
  "truncated": false,
  "worktrees": [
    {
      "branch": "main",
      "detached": false,
      "head": "48ce6b19c89fc67695e4e723b8192fb167fb8f00",
      "id": null,
      "is_bare": false,
      "is_current": true,
      "is_main": true,
      "lock_reason": null,
      "locked": false,
      "path": "/path/to/demo",
      "prunable": false
    },
    {
      "branch": "feature",
      "detached": false,
      "head": "e9383dc8fce1955c63812c2365edbff4efdffce4",
      "id": "demo-review",
      "is_bare": false,
      "is_current": false,
      "is_main": false,
      "lock_reason": "reviewing feature",
      "locked": true,
      "path": "/path/to/demo-review",
      "prunable": false
    }
  ]
}
```

## `list_submodules`

Submodules declared in `.gitmodules`, ordered by path, like `git submodule status`. For each one it reports the commit the superproject's index records, the commit checked out, and how they relate.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `limit` | integer | 100 (max 1000) | Maximum submodules to return. |
| `skip` | integer | 0 | Submodules to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `submodules[]` | array | Submodules ordered by path. Empty when there is no `.gitmodules`. |
| `submodules[].name` | string | Name from `.gitmodules`; often the same as `path`. |
| `submodules[].path` | string | Path relative to the repository root. |
| `submodules[].url` | string \| null | Configured URL (`.git/config` overrides `.gitmodules`), with any password redacted. Relative URLs such as `../lib` are returned as written. |
| `submodules[].branch` | string \| null | Branch the submodule follows (`submodule.<name>.branch`), or `.` for the superproject's current branch. |
| `submodules[].recorded_commit` | string \| null | Commit the superproject's index records; `null` when the path isn't in the index or is conflicted. |
| `submodules[].checked_out_commit` | string \| null | Commit checked out in the submodule; `null` when it isn't checked out. |
| `submodules[].state` | string | `uninitialized` (not checked out, `-` in `git submodule status`), `up_to_date`, `different_commit` (`+`), or `conflicted` (`U`). |
| `truncated` | boolean | True if more submodules exist past this page. |

**Example**

```json
{}
```

```json
{
  "submodules": [
    {
      "branch": null,
      "checked_out_commit": "666ff47cda0654e2425c5502e6db62804c86eb61",
      "name": "vendor/assets",
      "path": "vendor/assets",
      "recorded_commit": "666ff47cda0654e2425c5502e6db62804c86eb61",
      "state": "up_to_date",
      "url": "/path/to/assets"
    }
  ],
  "truncated": false
}
```

## `config_get`

Git configuration values with the scope and file each one comes from, like `git config --show-scope --show-origin`. Ask for one `key`, a whole `section`, or everything.

Values that may be secrets are replaced with `<redacted>` and flagged: anything under `credential`, keys whose name contains `password`, `passwd`, `token` or `secret`, `http.extraHeader`, and the password part of any URL (`https://user:<redacted>@host`). gix also turns some environment variables (such as `GIT_EDITOR` or `HTTPS_PROXY`) into configuration; like `git config --list`, this tool doesn't list those.

**Arguments**

| Argument | Type | Default | Description |
|---|---|---|---|
| `key` | string | none | A full key such as `user.email` or `remote.origin.url`; every value set for it is returned. Section and name are case-insensitive, the subsection in the middle isn't. |
| `section` | string | none | A section such as `core`, `remote` (every remote) or `remote.origin`, to list all of its keys. Omit both `key` and `section` to list everything. |
| `scope` | `system` \| `global` \| `local` \| `worktree` \| `command` | all | Only return values from this scope. |
| `limit` | integer | 100 (max 1000) | Maximum entries to return. |
| `skip` | integer | 0 | Entries to skip first, for pagination. |

**Result**

| Field | Type | Description |
|---|---|---|
| `entries[]` | array | Matching entries in the order git reads them: system, global, local, worktree, then command. When a single-valued key is set more than once, the last entry is the one git uses. |
| `entries[].key` | string | Full key, section and name in lowercase and the subsection as written, e.g. `remote.origin.url`. |
| `entries[].value` | string \| null | The value; `null` for a key written without `=`, which git reads as boolean true. |
| `entries[].scope` | string | `system`, `global` (`~/.gitconfig`), `local` (`.git/config`), `worktree` (`config.worktree`), or `command` (`GIT_CONFIG_COUNT` and `git -c` values). |
| `entries[].file` | string \| null | File the value was read from; `null` for values from the environment. |
| `entries[].redacted` | boolean | True when `value` was hidden because it may be a secret. |
| `truncated` | boolean | True if more entries exist past this page. |

**Errors:** both `key` and `section` given; a `key` without a section part: `` `key` must look like `section.name` or `section.subsection.name`, e.g. `user.email`; use `section` to list a whole section ``.

**Examples**

```json
{"section": "remote.origin"}
```

```json
{
  "entries": [
    {
      "file": "/path/to/demo/.git/config",
      "key": "remote.origin.url",
      "redacted": true,
      "scope": "local",
      "value": "https://ada:<redacted>@example.com/greet.git"
    },
    {
      "file": "/path/to/demo/.git/config",
      "key": "remote.origin.fetch",
      "redacted": false,
      "scope": "local",
      "value": "+refs/heads/*:refs/remotes/origin/*"
    }
  ],
  "truncated": false
}
```

```json
{"key": "core.bare"}
```

```json
{
  "entries": [
    {
      "file": "/path/to/demo/.git/config",
      "key": "core.bare",
      "redacted": false,
      "scope": "local",
      "value": "false"
    }
  ],
  "truncated": false
}
```

## `count_objects`

Statistics for the repository's object database, like `git count-objects -v`, with sizes in bytes instead of KiB.

**Arguments:** none.

**Result**

| Field | Type | Description |
|---|---|---|
| `loose_objects` | integer | Number of loose objects, one compressed file each under `.git/objects/xx/`. |
| `loose_bytes` | integer | Disk space the loose objects take, in bytes (allocated blocks, as git measures it). |
| `packed_objects` | integer | Objects in pack files, counted per pack: an object in two packs counts twice, as in git. |
| `packs` | integer | Number of pack files. |
| `pack_bytes` | integer | Total size of the pack files and their indexes, in bytes. |
| `commit_graph` | boolean | True when a commit-graph file exists, which speeds up history walks. |
| `multi_pack_index` | boolean | True when a multi-pack index exists. |
| `alternates` | integer | Number of other object directories this repository borrows objects from (`objects/info/alternates`). Their objects aren't counted. |

**Example**

```json
{}
```

```json
{
  "alternates": 0,
  "commit_graph": false,
  "loose_bytes": 118784,
  "loose_objects": 29,
  "multi_pack_index": false,
  "pack_bytes": 0,
  "packed_objects": 0,
  "packs": 0
}
```
