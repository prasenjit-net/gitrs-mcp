# Resources and completion

Besides tools, gitrs-mcp serves repository content as MCP **resources**: commits, files and diffs addressed by `gitrs://` URIs. A client can attach them to a conversation as context without the model calling a tool. Clients that support **completion** can also suggest revisions and paths while you fill in a resource's URI.

Resources are read-only and available with or without `--allow-write`.

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

## Completion

`completion/complete` suggests values for an argument from what has been typed so far. Values are sorted, prefix-matched and capped at 100; `total` counts every match, and `hasMore` says whether some were left out.

| Argument | Suggestions |
|---|---|
| `rev` | `HEAD`, branches, remote branches and tags, plus `index` and `worktree` |
| `from` | the same, plus `index` |
| `to` | the same, plus `index` and `worktree` |
| `path` | entries of the directory typed so far, at `rev` (or `from`) from the request's context, default `HEAD`; directories end in `/`. With `index` or `worktree`, tracked paths from the index. |

A context revision that doesn't resolve, such as one still being typed, gives no suggestions rather than an error.

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
