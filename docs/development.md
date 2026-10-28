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
| `make smoke` | End-to-end MCP round trip over stdio: initialize, `tools/list`, then one call to every read tool against `REPO`; then a second session with `--allow-write` that checks the write tools are listed only there and calls some of them in ways that can't change anything. Fails if any call has an unexpected outcome or gets no answer. |
| `make inspector` | Open the [MCP Inspector](https://github.com/modelcontextprotocol/inspector) against the server, to try tools by hand. |
| `make check` | `fmt-check`, `clippy`, `test` and `smoke`: everything CI should run. |
| `make coverage` | Line coverage summary; fails below 80% (`COVERAGE_MIN`). |
| `make stress` | Release build, then the heaviest calls at their largest limits on `REPO`, such as deep history, whole-tree listings and big diffs. Fails if any call errors or returns more than 4 MiB. |
| `make fuzz` | Runs each cargo-fuzz target in `fuzz/` for `FUZZ_TIME` seconds (default 60). Needs `rustup toolchain install nightly` and `cargo install cargo-fuzz`. |
| `make bench` | Release build, then times tool calls against the equivalent git commands on `REPO`, `RUNS` times each (default 5). Point it at a large repository such as git.git; see [Benchmarks](#benchmarks). |
| `make coverage-html` | Line-by-line HTML coverage report, opened in the browser. |
| `make install` / `make uninstall` | Install to or remove from `~/.cargo/bin`. |
| `make mcp-add` | Build a release binary and register it with Claude Code for `REPO`. |

Variables: `REPO=<path>` (default `.`), `ARGS="<extra server flags>"`, `LOG=<level>` (default `info`), `FILE=<tracked file>` for `make smoke` (default `README.md`), and `RUNS=<n>` for `make bench` (default 5). For example, `make smoke REPO=../other-project FILE=src/main.rs`.

## Adding a tool

A tool is one function and one wrapper. Using a made-up `describe` tool as the example:

1. **Pick the module.** Put the logic in the `tools/<area>.rs` file that fits, or create a new one and add it to `tools/mod.rs`. Check `tools/mod.rs` first for helpers you should reuse: `resolve_commit`, `resolve_tree`, `resolve_object`, `repo_path`, `page_limit`, `paginate`, `byte_budget`, `clip_utf8`, `LogEntry`, `Signature`, `ChangeKind`, `ObjectType`.

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

4. **Add the wrapper** to the router in `server/<area>.rs` that matches the tool's area (a new area gets its own file, and its router is added to the sum in `GitServer::new`). Its doc comment becomes the tool description, so say what the tool does, when to use it, and its key defaults or limits. Set a `title` and the correct hints: `read_only_hint = true` for reads, and `destructive_hint` / `idempotent_hint` for writes. Write tools must only be registered when `--allow-write` is set.

   ```rust
   /// Name a commit relative to the nearest tag, like `git describe`. ...
   #[tool(annotations(title = "Describe", read_only_hint = true))]
   async fn describe(
       &self,
       Parameters(InRepo { repo, args }): Parameters<InRepo<tools::refs::DescribeArgs>>,
   ) -> Result<Json<tools::refs::DescribeResult>, String> {
       self.repo(repo)?
           .run(move |r| tools::refs::describe(r, args))
           .await
           .map(Json)
   }
   ```

   `InRepo` adds the optional `repo` argument every tool takes. A tool without arguments takes `Parameters(RepoOnly { repo }): Parameters<RepoOnly>` instead. Write tools select their repository with `self.writable(repo)?`, which refuses repositories you don't own.

5. **Test it** in the same file (see below), and add a call to `scripts/smoke.sh`. If the call needs something the served repository may not have (a tag, a merge in progress), ask for something that can't exist and add the request id to `expect_error`: the smoke test then requires that call to return a tool error.

6. **Document it** in the same change: a section in [tools.md](tools.md) with an example taken from real output (build the demo workspace with `scripts/demo-repo.sh <new dir>` and call the tool on the repository it creates, for example with `make inspector REPO=<dir>/demo`), a row in the README's tool table, and the tool marked done in [ROADMAP.md](../ROADMAP.md).

7. **Record the API.** Run `make api` to add the tool to `api/schema.json`. The API test fails until you do, so the new schema shows up in review. Renaming or removing anything is a breaking change; see [versioning](versioning.md).

## Testing

Every tool has unit tests in its own file, in a `#[cfg(test)] mod tests` block. Shared fixtures live in `tools::testutil`; add to it rather than creating a second fixture builder.

| Helper | Gives you |
|---|---|
| `fixture_repo()` | A temp repo on `main` with two commits to `a.txt`, a tag `v1` and a branch `feature`. |
| `empty_repo()` | An initialized repo with no commits. |
| `bare_repo()` | A bare clone of `fixture_repo()`. |
| `open(path)` | Open a repo with global and system git config ignored, so a developer's `~/.gitconfig` can't change results. |
| `run_git`, `run_git_with_env`, `try_git`, `git_stdout` | Run the system `git` with a clean, fixed identity, to build scenarios or get the expected output. |
| `git_output` | Run `git` with input on stdin and return its raw output without checking the exit status, for oracles such as `git grep` and `git check-ignore` that exit 1 when nothing matches. |
| `write(root, path, content)` | Write a file, creating parent directories. |
| `snapshot(dir)` | Every file below `dir` with its content (symlinks by target), to prove a refused or dry-run write changed nothing. |

**Use git as the oracle.** When a tool's output mirrors a git command, build the scenario with `git`, run the matching command, and assert that the tool's result is equivalent. The existing tests compare against `git status --porcelain=v2`, `git diff` (`--numstat`, `--name-status` and full patch text), `git ls-tree`, `git log -- <path>`, `git log --follow`, `git log -g`, `git rev-list`, `git rev-parse`, `git merge-base`, `git blame --porcelain`, `git describe`, `git stash list`, `git notes`, `git grep`, `git check-ignore`, `git check-attr`, `git config --list`, `git worktree list --porcelain`, `git submodule status`, `git count-objects -v`, `git ls-files -u` and `git merge-tree --write-tree`. `merge_preview` is also checked to leave `.git/objects` untouched. The diff tests also apply generated patches to a clone with `git apply --index` and check that the result matches the target commit.

**Integration tests spawn the binary.** `tests/stdio.rs` starts the real `gitrs-mcp` process with rmcp's child-process transport and talks to it over stdio. It covers what only the process shows: argument parsing, `--allow-write`, several `--repo` flags, launching from a subdirectory, and failing to start outside a repository. It's a separate crate, which can't reach `tools::testutil`, so it builds its own two-commit fixture. Keep anything more elaborate in unit tests.

**Test the MCP surface through the protocol.** For anything that isn't a tool (resources, completion, prompts), `server::tests::connect` starts the server on an in-memory pipe and returns an rmcp client (the `client` feature is a dev-dependency only), so the test sends real requests such as `read_resource` or `complete`. Resource content is checked against git as well: a commit resource must equal `git show`, and a diff resource `git diff`.

**Cover the edges.** For each tool, test the happy path and its defaults (including deserializing `{}`), plus the cases that apply: an empty repository, a detached HEAD, a bare repository, unicode paths, binary content, pagination boundaries, bad revisions, missing paths and refused input. Write tools must also test that every refused or dry-run call leaves the repository byte-for-byte unchanged: take `testutil::snapshot(root)` (every file under the worktree and `.git`) before the call and compare it afterwards. Fixture repositories have `user.name` and `user.email` in their local config, because repositories opened by `open` ignore the environment and write tools need a committer.

**Fix bugs test-first.** Start a bug fix with a failing test that reproduces it.

## Coverage

`make coverage` fails below 80% line coverage. New code in `src/tools/` should be at or near full coverage, and coverage must not drop. `cargo llvm-cov` also instruments the binary the integration tests spawn, so `main.rs` and start-up are covered. Everything else in `server/` (write tools, resources, prompts, progress, subscriptions, several repositories and roots) is tested through the protocol with `server::tests::connect`, since a stdio smoke test can't safely change the served repository. Use `make coverage-html` to see which lines a change left untested.

## Fuzzing and property tests

Everything that parses text from a tool call is tested two ways:

- **Property tests** run on stable as part of `make test`. They use [proptest](https://crates.io/crates/proptest), and live next to the code they cover:
  - normalized paths never escape and normalize to themselves;
  - every spelling of `.git` is refused, and nothing else is;
  - resource URIs and `?repo=` names round-trip;
  - dates and revisions never panic.
- **Fuzz targets** live in `fuzz/` and run with libFuzzer through `make fuzz`. They need a nightly toolchain:
  - `paths`, which also checks that an accepted path can't escape;
  - `resource_uris`;
  - `dates`;
  - `revisions`, resolved in the repository named by `FUZZ_REPO`, by default this checkout.

  The targets link against the library half of the crate (`src/lib.rs`) through its `#[doc(hidden)] fuzzing` module. Add an entry point there for any new parser. The `fuzz/` crate has its own workspace, so stable builds never see it.

On the first runs, the property tests found that `./\x` normalized to the Windows-absolute `\x`. One minute per fuzz target, about 3.7 million inputs in all, found nothing more.

## Stress tests

`make stress REPO=<large repository>` sends the heaviest calls once each, at their largest limits. On git.git, every call succeeded within its bounds; the slowest were `blame` (1.1 s) and `compare_refs` across ten years of history (1.1 s). `make smoke REPO=<git.git> FILE=Makefile` also passes there. linux.git (about 5 GB) wasn't tried. The byte budgets and page limits are what bound output, and they don't depend on repository size.

## Benchmarks

`make bench REPO=<path>` (`scripts/bench.py`) starts the release server once and times tool calls from request to response, then the equivalent git commands as fresh processes. The first call of each case runs with cold caches, and the best of the rest with warm ones. Cases that need a file or author missing from the repository are skipped.

On a clone of git.git (82,605 commits, git 2.50.1, Apple M-series laptop, 5 runs each):

| Case | gitrs-mcp first call | gitrs-mcp best | git best |
|---|---:|---:|---:|
| log, 500 commits | 40 ms | 18 ms | 11 ms |
| log, 20 after skipping 20000 | 84 ms | 84 ms | 96 ms |
| search_commits by author, 50 | 3 ms | 2 ms | 8 ms |
| file_history Makefile, 100 | 57 ms | 39 ms | 23 ms |
| blame diff.c | 1,266 ms | 1,090 ms | 739 ms |
| grep 'static int', 100 matches | 88 ms | 27 ms | 94 ms |

- **`log`, `search_commits` and `grep`** are on a par with git or faster. git pays for starting a process on every call, and the server is already running.
- **`file_history`** is about 1.7 times git. It's our own walk, implementing git's history simplification, without a changed-path Bloom filter.
- **`blame`** is about 1.5 times git. That time is gix's blame algorithm.
- **First calls** are slower mostly because the operating system's file cache is cold. The per-thread object cache (see [architecture](architecture.md#the-syncasync-bridge)) makes repeated calls about 10% faster than without it: 22 to 18 ms for `log`, and 1,193 to 1,090 ms for `blame`.

## Releasing

1. Set the version in `Cargo.toml`, following [versioning](versioning.md).
2. Move the `Unreleased` entries in [CHANGELOG.md](../CHANGELOG.md) under the new version.
3. Make sure `api/schema.json` is current (`make api`) and that `make check` passes.

## Before committing

Run `make check`, then `make coverage`, then review `git diff --staged` for duplicated logic, leftover debug output, `unwrap()` outside tests, missing doc comments, and docs that weren't updated. The full checklist is in [CLAUDE.md](../CLAUDE.md).
