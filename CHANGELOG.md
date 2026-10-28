# Changelog

All notable changes to gitrs-mcp are listed here. The project follows [semantic versioning](https://semver.org); [docs/versioning.md](docs/versioning.md) says what counts as a breaking change.

## Unreleased

## 0.1.0

The first release.

- **Read tools (28):**
  - repository information, refs, tags, reflogs, stashes and notes;
  - status, diffs, conflicts and merge previews;
  - file contents, trees, `grep`, ignore and attribute rules;
  - history, file history, commit search and blame;
  - worktrees, submodules, config (with secrets redacted) and object statistics;
  - the served repositories.
- **Write tools (22), only with `--allow-write`:**
  - branches, tags, staging and commits;
  - reset (soft, mixed, and a hard reset that dry-runs by default), restore, switch and clean;
  - stashes;
  - merge, cherry-pick, revert and rebase, with continue and abort, which leave git-compatible state so git can finish what gitrs-mcp started, and the reverse;
  - adding and removing worktrees.
- **Resources:** `gitrs://commit/{rev}`, `gitrs://file/{rev}/{+path}` and `gitrs://diff/{from}..{to}`, with completion of revisions and paths.
- **Prompts:** `write_commit_message`, `review_changes` and `explain_history`.
- **Progress and cancellation** for long walks, and resource change notifications with `--watch`.
- **Several repositories** per server: repeat `--repo`, or let the client's MCP roots add them. Every tool, prompt and resource takes an optional `repo`.
- **Local only:** gix is built without network support. Repositories you don't own are read-only, as with git's `safe.directory`. See [docs/security.md](docs/security.md).
