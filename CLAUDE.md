# gitrs-mcp: project instructions

gitrs-mcp is a local-only Git MCP server in Rust, built on `gix` (gitoxide) and `rmcp`, speaking MCP over stdio. [ROADMAP.md](ROADMAP.md) has the plan and the tool specs. These rules apply to every change.

## 1. Code quality

- **Readable first.** Use small functions with one job, descriptive names, and early returns instead of deep nesting. Code should read top-down without needing comments to explain *what* it does. Comments explain *why*: non-obvious git semantics, gix quirks, safety decisions.
- **No duplication.** Before writing a helper, search for an existing one (`src/tools/mod.rs`, `src/repo.rs`). If logic appears twice, extract it.
  - Shared output types such as `Signature` and entries for commits or refs live in `tools/mod.rs`, or in a shared module once one exists.
  - Shared argument handling such as rev defaults, pagination and byte budgets gets one helper, used everywhere.
  - Test fixtures and git-oracle helpers live in `tools::testutil`. Don't create a second fixture builder.
- **Design principles.**
  - *Single responsibility, by layer.* `main.rs` handles CLI and transport. `server/` handles MCP registration only: thin wrappers with no git logic, one router per area. `repo.rs` bridges sync gix calls to async. `tools/<area>.rs` holds git logic as plain sync functions over `&gix::Repository`.
  - *Dependency direction.* `tools` never imports `rmcp` or `server`, and `server` never touches gix internals directly.
  - *Open/closed.* Add a tool by adding a function and a wrapper. Don't edit unrelated tools.
  - *Explicit over clever.* Use typed args/result structs, not `serde_json::Value`. No `unwrap()`/`expect()` outside tests. Propagate errors with `?` and `anyhow::Context`.
  - Keep public surface minimal: `pub(crate)` unless something truly needs to be public.
- **Style gate.** `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must be clean. Never silence a lint with `#[allow]` without a comment explaining why.

## 2. MCP usability

The model only knows what the schema and descriptions tell it, so the documentation is part of the API.

- **Tool descriptions.** Every `#[tool]` has a doc comment stating what the tool does, when to use it, and the key defaults or limits.
- **Field descriptions.** Every field of every args and result struct has a `///` doc comment. These become the JSON Schema descriptions. State defaults, units (seconds, bytes), formats (full SHA vs short) and what `null` means.
- **Annotations.** Set `title` and the correct hints: `read_only_hint` for reads, and `destructive_hint` / `idempotent_hint` for writes.
- **Errors.** Return errors as tool errors (`Err(String)` via `RepoHandle::run`) with messages that say what was wrong *and* how to fix it.
- **Bounded output.** Every list is paginated (`limit`, `skip`, or a cursor) and reports `truncated`. Large content has a byte budget.
- **Safety.** Write tools register only with `--allow-write`. Destructive tools default to `dry_run: true`. Never add network features to gix.
- **Stability.** Tool names and schemas are a public API. Add optional fields freely, but don't rename or remove anything without flagging it as a breaking change.

## 3. Documentation, updated in the same change as the code

- **README.md.** Update it whenever user-visible behaviour changes: the tool table, CLI flags, install or usage steps, make targets.
- **docs/.** Shareable project documentation for users and contributors, written so it makes sense to someone who hasn't seen this conversation or the code. Create or update the relevant page with every feature:
  - `docs/tools.md`: full reference for every tool, covering purpose, arguments with defaults, result fields, errors, and a JSON request/response example.
  - `docs/architecture.md`: layers, data flow, the sync/async bridge, and the safety model.
  - `docs/development.md`: setup, the make targets, how to add a tool, testing and coverage.
  - `docs/configuration.md`: CLI flags, env vars, and client setup (Claude Code, Claude Desktop, others).
- **ROADMAP.md.** Mark tools as done when they ship. Note any deviation from the planned design and the reason.
- Examples in the docs must be real output from running the tool (for example via `make smoke` or `make inspector`), not invented.

## 4. Testing

- Every tool function gets unit tests in its own file (`#[cfg(test)] mod tests`). Cover:
  - the happy path and the defaults;
  - edge cases: an empty repo, detached HEAD, bare repo, unicode paths, binary content, pagination boundaries;
  - error paths: a bad revision, a missing path, a refused write.
- Use system `git` as the oracle. When output mirrors a git command, assert it matches that command's output on the same fixture.
- Write tools additionally test that a refused or dry-run call leaves the repo byte-for-byte unchanged.
- A bug fix starts with a failing test that reproduces it.
- **Coverage.** `make coverage` must pass. It fails below **80% line coverage**. New code in `src/tools/` should be at or near full coverage, and coverage must not drop. It needs `cargo install cargo-llvm-cov`.

## 5. Before every commit and push

Run, in order, and fix everything before committing:

1. `make check`: format check, clippy (warnings are errors), tests, and the MCP smoke test.
2. `make coverage`: the coverage threshold.
3. **Review the diff yourself** (`git diff --staged`). Look for:
   - duplicated logic, dead code, leftover debug output, `unwrap()` outside tests;
   - missing doc comments on tools or fields;
   - README, docs/ or ROADMAP.md not updated;
   - secrets, absolute local paths, or stray files (build output, scratch files).
4. Commit with a message that says *what* changed and *why*, one logical change per commit.
5. Push only when the user asks, and never force-push `master`.
