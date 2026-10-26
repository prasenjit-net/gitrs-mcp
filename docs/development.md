# Development

How to build, test and extend gitrs-mcp. The project rules (code quality, MCP documentation, testing, and the pre-commit checklist) are in [CLAUDE.md](../CLAUDE.md); this page is the practical companion.

## Setup

You need:

- **Rust 1.85 or newer** (the crate uses edition 2024).
- **`git`** on your `PATH`. The server itself never runs it, but the tests use it to build fixture repositories and as an oracle for the expected output.
- **`python3`** for the smoke test, which parses the server's JSON-RPC output.
- **cargo-llvm-cov** for coverage:

  ```sh
  rustup component add llvm-tools-preview
  cargo install cargo-llvm-cov --locked
  ```

- Optionally **npx** (Node.js) for `make inspector`.

Then:

```sh
git clone https://github.com/prasenjit-net/gitrs-mcp.git
cd gitrs-mcp
make build
make check
```

## Make targets

Run `make` (or `make help`) for the full list.

| Target | What it does |
|---|---|
| `make build` / `make release` | Debug or optimized build. |
| `make run` | Run the server read-only over stdio on `REPO`. |
| `make run-write` | Run with `--allow-write`. |
| `make test` | Unit tests. `make test-verbose` shows test output. |
| `make fmt` / `make fmt-check` | Format, or check formatting. |
| `make clippy` | Lint with warnings as errors. |
| `make smoke` | End-to-end MCP round trip over stdio: initialize, `tools/list`, then one call to every tool against `REPO`. Fails if any call errors or gets no answer. |
| `make inspector` | Open the [MCP Inspector](https://github.com/modelcontextprotocol/inspector) against the server, to try tools by hand. |
| `make check` | `fmt-check`, `clippy`, `test` and `smoke`: everything CI should run. |
| `make coverage` | Line coverage summary; fails below 80% (`COVERAGE_MIN`). |
| `make coverage-html` | Line-by-line HTML coverage report, opened in the browser. |
| `make install` / `make uninstall` | Install to or remove from `~/.cargo/bin`. |
| `make mcp-add` | Build a release binary and register it with Claude Code for `REPO`. |

Variables: `REPO=<path>` (default `.`), `ARGS="<extra server flags>"`, `LOG=<level>` (default `info`), and `FILE=<tracked file>` for `make smoke` (default `README.md`). For example, `make smoke REPO=../other-project FILE=src/main.rs`.

## Adding a tool

A tool is one function and one wrapper. Using a made-up `describe` tool as the example:

1. **Pick the module.** Put the logic in the `tools/<area>.rs` file that fits, or create a new one and add it to `tools/mod.rs`. Check `tools/mod.rs` first for helpers you should reuse: `resolve_commit`, `resolve_tree`, `repo_path`, `page_limit`, `paginate`, `byte_budget`, `LogEntry`, `Signature`, `ChangeKind`.

2. **Define the argument and result types.** Arguments derive `Deserialize + JsonSchema`, results derive `Serialize + JsonSchema`. Put a `///` comment on every field: these become the JSON Schema descriptions, which are all the model knows about the tool. State defaults, units, formats and what `null` means.

   ```rust
   #[derive(Debug, Deserialize, JsonSchema)]
   pub struct DescribeArgs {
       /// Commit to describe (default `HEAD`).
       pub rev: Option<String>,
   }

   #[derive(Debug, Serialize, JsonSchema)]
   pub struct DescribeResult {
       /// Description such as `v1.2-3-gabc1234`, like `git describe`.
       pub name: String,
   }
   ```

3. **Write the function.** It takes `&gix::Repository` and the args, and returns `anyhow::Result<DescribeResult>`. Return errors with `?` and `anyhow::Context`, never `unwrap()`. Write error messages that say what was wrong and how to fix it. Lists need `limit` and `truncated`; content needs a byte budget.

4. **Add the wrapper** in `server.rs`. Its doc comment becomes the tool description, so say what the tool does, when to use it, and its key defaults or limits. Set a `title` and the correct hints: `read_only_hint = true` for reads, and `destructive_hint` / `idempotent_hint` for writes. Write tools must only be registered when `--allow-write` is set.

   ```rust
   /// Name a commit relative to the nearest tag, like `git describe`. ...
   #[tool(annotations(title = "Describe", read_only_hint = true))]
   async fn describe(
       &self,
       Parameters(args): Parameters<tools::refs::DescribeArgs>,
   ) -> Result<Json<tools::refs::DescribeResult>, String> {
       self.repo.run(move |r| tools::refs::describe(r, args)).await.map(Json)
   }
   ```

5. **Test it** in the same file (see below), and add a call to `scripts/smoke.sh`.

6. **Document it** in the same change: a section in [tools.md](tools.md) with an example taken from real output, a row in the README's tool table, and the tool marked done in [ROADMAP.md](../ROADMAP.md).

## Testing

Every tool has unit tests in its own file, in a `#[cfg(test)] mod tests` block. Shared fixtures live in `tools::testutil`; add to it rather than creating a second fixture builder.

| Helper | Gives you |
|---|---|
| `fixture_repo()` | A temp repo on `main` with two commits to `a.txt`, a tag `v1` and a branch `feature`. |
| `empty_repo()` | An initialized repo with no commits. |
| `bare_repo()` | A bare clone of `fixture_repo()`. |
| `open(path)` | Open a repo with global and system git config ignored, so a developer's `~/.gitconfig` can't change results. |
| `run_git`, `run_git_with_env`, `try_git`, `git_stdout` | Run the system `git` with a clean, fixed identity, to build scenarios or get the expected output. |
| `write(root, path, content)` | Write a file, creating parent directories. |

**Use git as the oracle.** When a tool's output mirrors a git command, build the scenario with `git`, run the matching command, and assert that the tool's result is equivalent. The existing tests compare against `git status --porcelain=v2`, `git diff` (`--numstat`, `--name-status` and full patch text), `git ls-tree`, `git log -- <path>`, `git log --follow`, `git rev-list`, `git rev-parse`, `git merge-base` and `git blame --porcelain`. The diff tests also apply generated patches to a clone with `git apply --index` and check that the result matches the target commit.

**Cover the edges.** For each tool, test the happy path and its defaults (including deserializing `{}`), plus the cases that apply: an empty repository, a detached HEAD, a bare repository, unicode paths, binary content, pagination boundaries, bad revisions, missing paths and refused input. Write tools will also need to test that a refused or dry-run call leaves the repository byte-for-byte unchanged.

**Fix bugs test-first.** Start a bug fix with a failing test that reproduces it.

## Coverage

`make coverage` fails below 80% line coverage. New code in `src/tools/` should be at or near full coverage, and coverage must not drop. `main.rs`, `server.rs` and `repo.rs` are exercised by `make smoke`, not by unit tests, so they show as uncovered there. Use `make coverage-html` to see which lines a change left untested.

## Before committing

Run `make check`, then `make coverage`, then review `git diff --staged` for duplicated logic, leftover debug output, `unwrap()` outside tests, missing doc comments, and docs that weren't updated. The full checklist is in [CLAUDE.md](../CLAUDE.md).
