# Resources, prompts and completion

Besides tools, gitrs-mcp offers three other MCP features:

- **Resources:** repository content (commits, files and diffs) addressed by `gitrs://` URIs. A client can attach them to a conversation as context without the model calling a tool.
- **Prompts:** ready-made requests, such as "write a commit message", that come filled in with the repository data they need. Clients usually offer them as slash commands.
- **Completion:** suggestions for revisions and paths while you fill in a resource URI or a prompt argument.

All three are read-only and available with or without `--allow-write`. With `--watch`, clients can also subscribe to resources and get notified when they change.

## Listed resources

`resources/list` returns three resources that are always useful. The staged and unstaged ones appear only in a repository with a working tree.

| URI | Title | Content |
|---|---|---|
| `gitrs://commit/HEAD` | HEAD commit | The commit HEAD points to, with its patch |
| `gitrs://diff/HEAD..index` | Staged changes | What the next commit would contain, like `git diff --cached` |
| `gitrs://diff/index..worktree` | Unstaged changes | Changes in the working tree that aren't staged, like `git diff` |

## Templates

`resources/templates/list` returns three URI templates. Any resource built from them can be read with `resources/read`.

| Template | MIME type | Content |
|---|---|---|
| `gitrs://commit/{rev}` | `text/x-diff` | A commit as `git show` prints it: header, indented message, and the patch against its first parent |
| `gitrs://file/{rev}/{+path}` | `text/plain`, or `application/octet-stream` for binary files | A file's content |
| `gitrs://diff/{from}..{to}` | `text/x-diff` | A unified diff, like `git diff` |

**Several repositories.** Each template ends in `{?repo}`: add `?repo=<name>`, using a name from [`list_repos`](tools.md#list_repos), to read from a served repository other than the default. For example, `gitrs://commit/HEAD?repo=api` reads `api`'s HEAD commit. When several repositories are served, the listed resources of the others carry their `?repo=`.

**Revisions** (`rev`, `from`, `to`) accept anything `git rev-parse` would: branches, tags, SHAs and expressions such as `HEAD~2`. Percent-encode a `/` inside a revision as `%2F`, so branch `topic/x` is `gitrs://commit/topic%2Fx`.

- **`commit`**
  - The output matches `git show <rev>` byte for byte, with paths printed unquoted as with `core.quotePath=false`.
  - A merge commit shows its `Merge:` line and the patch against its **first parent**. Git would show a combined diff, which can't be applied.
- **`file`**
  - `rev` can also be `index` for the staged version, or `worktree` for the file on disk. A branch literally named `index` or `worktree` can be read as `refs/heads/index`.
  - The path is relative to the repository root and may contain `/`. Percent-encoding in it is decoded.
  - Text comes back as text, and binary content as a base64 blob.
  - Files over 1 MiB are refused; the [`read_file`](tools.md#read_file) tool reads them in chunks.
- **`diff`**
  - Supported pairs:
    - `<rev>..<rev>`: between two revisions.
    - `<rev>..index`: changes staged on top of a revision.
    - `index..worktree`: unstaged changes.
  - Comparing a revision directly with the working tree isn't supported, as in the [`diff`](tools.md#diff) tool.
  - A diff (or a commit's patch) covers up to 1,000 files and 1 MiB of patch text. If anything is left out, a closing `#` line says how many files, and the `diff` tool pages through them. `git apply` ignores the note.
  - Paths with unresolved conflicts aren't diffed. A closing `# Unmerged, not diffed:` line names them.

### Errors

A URI that can't be read comes back as a JSON-RPC error with code `-32002` (resource not found). The message says what was wrong and how to fix it:

- a URI that doesn't start with `gitrs://`, or an unknown kind;
- a missing revision or path;
- an unsupported diff pair;
- a revision that doesn't resolve, or a path that doesn't exist;
- a file over 1 MiB.

### Examples

Run against the demo repository from `scripts/demo-repo.sh`.

```json
{"method": "resources/read", "params": {"uri": "gitrs://commit/HEAD~1"}}
```

```json
{
  "contents": [
    {
      "uri": "gitrs://commit/HEAD~1",
      "mimeType": "text/x-diff",
      "text": "commit 41dad034dd469fe144a58a71bcd86453cd70ade1\nAuthor: Ada Lovelace <ada@example.com>\nDate:   Fri Oct 2 10:30:00 2026 +0000\n\n    Add shout\n    \n    Uppercase variant of greet.\n\ndiff --git a/src/lib.rs b/src/lib.rs\nindex 4aacacf..b1a7078 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,7 @@\n pub fn greet(name: &str) -> String {\n     format!(\"Hello, {name}!\")\n }\n+\n+pub fn shout(name: &str) -> String {\n+    greet(name).to_uppercase()\n+}\n"
    }
  ]
}
```

```json
{"method": "resources/read", "params": {"uri": "gitrs://diff/HEAD..index"}}
```

```json
{
  "contents": [
    {
      "uri": "gitrs://diff/HEAD..index",
      "mimeType": "text/x-diff",
      "text": "diff --git a/src/main.rs b/src/main.rs\nnew file mode 100644\nindex 0000000..fa49d50\n--- /dev/null\n+++ b/src/main.rs\n@@ -0,0 +1,3 @@\n+fn main() {\n+    println!(\"{}\", greet::greet(\"world\"));\n+}\n"
    }
  ]
}
```

```json
{"method": "resources/read", "params": {"uri": "gitrs://file/main/src/lib.rs"}}
```

```json
{
  "contents": [
    {
      "uri": "gitrs://file/main/src/lib.rs",
      "mimeType": "text/plain",
      "text": "pub fn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n\npub fn shout(name: &str) -> String {\n    greet(name).to_uppercase()\n}\n"
    }
  ]
}
```

```json
{"method": "resources/read", "params": {"uri": "gitrs://diff/HEAD..worktree"}}
```

```json
{
  "code": -32002,
  "message": "unsupported diff `HEAD..worktree`; use `<rev>..<rev>`, `<rev>..index` or `index..worktree`"
}
```

## Subscriptions

With [`--watch`](configuration.md#command-line), clients can subscribe to any `gitrs://` resource, listed or built from a template. The server then sends `notifications/resources/updated` when the resource changes, and the client reads it again. Without `--watch`, the `resources.subscribe` capability isn't advertised and subscribing is refused.

Both MCP subscription styles work:

- **`resources/subscribe` and `resources/unsubscribe`**, which current clients use with the `initialize` handshake. Subscribing twice to the same URI is one subscription. A URI that isn't a valid `gitrs://` resource is refused with code `-32602`.
- **`subscriptions/listen`**, the stream of protocol 2026-07-28. The acknowledgment lists the URIs that were accepted; those that don't parse are left out. Cancelling the stream ends the subscription.

A background task checks each subscribed URI once a second. It compares a cheap fingerprint of what the resource shows:

- a revision: the object it resolves to now, so a commit, a moved branch or a new tag all count;
- `index`: the index file's size and modification time, so staging counts;
- `worktree`: the resource's content, since edits on disk change nothing in HEAD, refs or the index. A subscription to `gitrs://diff/index..worktree` therefore computes that diff once a second.

A URI that becomes readable or unreadable, such as a revision that starts or stops existing, counts as a change too. Changes that happen in quick succession within one second produce one notification.

## Prompts

`prompts/list` returns three prompts. Each also takes an optional `repo` argument, which picks the served repository. `prompts/get` returns one user message with the instructions and the data filled in. Code and diffs are inside Markdown code fences, which are longer than any backtick run in the content. Prompt arguments are strings, as MCP defines them.

| Prompt | Arguments | Fills in |
|---|---|---|
| `write_commit_message` | none | The staged diff (`git diff --cached`) and the subjects of the last 10 commits as a style guide (none before the first commit) |
| `review_changes` | `from?` (default `HEAD`), `to?` (default `index`) | The diff between the two sides, and the blame of the lines each hunk replaces |
| `explain_history` | `path`, `limit?` (default 20, max 100) | The file's commits, following renames, and the full message and patch of the 10 most recent |

- **`review_changes`**
  - The sides are the pairs the [diff resource](#templates) supports. The default `HEAD..index` reviews the staged changes, and `from: index, to: worktree` the unstaged ones.
  - For each modified or renamed file (up to 20 files and 10 hunks each), it blames the old lines of every hunk, including the hunk's context lines, at `from`. When `from` is `index`, it blames at `HEAD` instead, since the index isn't a commit. Each blamed range names the commit, its summary, author and date, so the review can check that the change fits the original intent.
- **Budgets.** Patch text in a prompt stops at 128 KiB. `explain_history` stops adding patches once the budget runs out, and says so.

### Errors

A prompt that can't be built comes back as a JSON-RPC error with code `-32602` (invalid params). The message says what to change:

- nothing is staged, for `write_commit_message`;
- no changes between the two sides, or an unsupported pair, for `review_changes`;
- a path with no history, or a `limit` that isn't a positive number, for `explain_history`;
- a revision that doesn't resolve.

### Examples

Run against the demo repository from `scripts/demo-repo.sh`, which has `src/main.rs` staged and an unstaged edit to `src/lib.rs`.

```json
{"method": "prompts/get", "params": {"name": "write_commit_message"}}
```

```json
{
  "description": "Commit message for the staged changes",
  "messages": [
    {
      "role": "user",
      "content": {
        "type": "text",
        "text": "Write a git commit message for the staged changes below. Start with a summary line of about 50 characters in the imperative mood, then a blank line and a body, wrapped at 72 columns, that explains what changed and why. Reply with the message only.\n\nMatch the style of this repository's recent commit subjects:\n\n- Document usage\n- Add shout\n- Initial commit\n\nStaged changes (`git diff --cached`):\n\n```diff\ndiff --git a/src/main.rs b/src/main.rs\nnew file mode 100644\nindex 0000000..fa49d50\n--- /dev/null\n+++ b/src/main.rs\n@@ -0,0 +1,3 @@\n+fn main() {\n+    println!(\"{}\", greet::greet(\"world\"));\n+}\n```\n"
      }
    }
  ]
}
```

```json
{"method": "prompts/get", "params": {"name": "review_changes", "arguments": {"from": "index", "to": "worktree"}}}
```

```json
{
  "description": "Code review of a diff, with blame context",
  "messages": [
    {
      "role": "user",
      "content": {
        "type": "text",
        "text": "Review the changes below (`index..worktree`) as an experienced reviewer would. Point out bugs, unhandled edge cases, security problems, and code that is hard to read or maintain, citing file and line. Then say briefly what is good about the change. The blame shows which commits last touched the lines each hunk replaces; use their summaries to judge whether the change respects the original intent.\n\nDiff:\n\n```diff\ndiff --git a/src/lib.rs b/src/lib.rs\nindex b1a7078..de41029 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,5 +1,5 @@\n pub fn greet(name: &str) -> String {\n-    format!(\"Hello, {name}!\")\n+    format!(\"Hello, {name}!!\")\n }\n \n pub fn shout(name: &str) -> String {\n```\n\nBlame of the replaced lines, at `HEAD`:\n\n- src/lib.rs lines 1-3: 73656d92804c \"Initial commit\" by Ada Lovelace, 2026-10-01\n- src/lib.rs lines 4-5: 41dad034dd46 \"Add shout\" by Ada Lovelace, 2026-10-02\n"
      }
    }
  ]
}
```

```json
{"method": "prompts/get", "params": {"name": "explain_history", "arguments": {"path": "src/lib.rs", "limit": "5"}}}
```

```json
{
  "description": "How a file evolved, from its history",
  "messages": [
    {
      "role": "user",
      "content": {
        "type": "text",
        "text": "Explain how `src/lib.rs` evolved and why, using its history below: the main phases of its development, the reasons behind significant changes (from the commit messages), and anything a newcomer should know before changing it.\n\nCommits that changed it, newest first:\n\n- 41dad03 modified: \"Add shout\" by Ada Lovelace, 2026-10-02\n- 73656d9 added: \"Initial commit\" by Ada Lovelace, 2026-10-01\n\nThe most recent changes in detail:\n\n### 41dad03 by Ada Lovelace, 2026-10-02\n\nAdd shout\n\nUppercase variant of greet.\n\n```diff\ndiff --git a/src/lib.rs b/src/lib.rs\nindex 4aacacf..b1a7078 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,7 @@\n pub fn greet(name: &str) -> String {\n     format!(\"Hello, {name}!\")\n }\n+\n+pub fn shout(name: &str) -> String {\n+    greet(name).to_uppercase()\n+}\n```\n\n### 73656d9 by Ada Lovelace, 2026-10-01\n\nInitial commit\n\n```diff\ndiff --git a/src/lib.rs b/src/lib.rs\nnew file mode 100644\nindex 0000000..4aacacf\n--- /dev/null\n+++ b/src/lib.rs\n@@ -0,0 +1,3 @@\n+pub fn greet(name: &str) -> String {\n+    format!(\"Hello, {name}!\")\n+}\n```\n"
      }
    }
  ]
}
```

## Completion

`completion/complete` suggests values for an argument of a resource template or a prompt, from what has been typed so far. Values are sorted, prefix-matched and capped at 100; `total` counts every match, and `hasMore` says whether some were left out.

| Argument | Suggestions |
|---|---|
| `rev` | `HEAD`, branches, remote branches and tags, plus `index` and `worktree` |
| `from` | the same, plus `index` |
| `to` | the same, plus `index` and `worktree` |
| `repo` | names of the served repositories |
| `path` | entries of the directory typed so far, at `rev` (or `from`) from the request's context, default `HEAD`; directories end in `/`. With `index` or `worktree`, tracked paths from the index. |

A `repo` in the request's context picks the repository that revisions and paths complete in. A context revision that doesn't resolve, such as one still being typed, gives no suggestions rather than an error.

### Examples

```json
{"method": "completion/complete", "params": {"ref": {"type": "ref/resource", "uri": "gitrs://file/{rev}/{+path}"}, "argument": {"name": "path", "value": "src/"}, "context": {"arguments": {"rev": "main"}}}}
```

```json
{
  "completion": {
    "values": [
      "src/lib.rs"
    ],
    "total": 1,
    "hasMore": false
  }
}
```

```json
{"method": "completion/complete", "params": {"ref": {"type": "ref/resource", "uri": "gitrs://diff/{from}..{to}"}, "argument": {"name": "to", "value": ""}}}
```

```json
{
  "completion": {
    "values": [
      "HEAD",
      "feature",
      "index",
      "main",
      "polish",
      "v0.1.0",
      "worktree"
    ],
    "total": 7,
    "hasMore": false
  }
}
```
