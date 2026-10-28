# gitrs-mcp plan

The six build phases are done: 28 read-only tools, 22 write tools behind `--allow-write`, resources, prompts, several repositories, a security review, fuzzing and API versioning. The old roadmap, with the per-phase tool specs and deviations, is in git history: `git show e2b0010:ROADMAP.md`. The living parts moved to [docs/tools.md](docs/tools.md) and [docs/architecture.md](docs/architecture.md).

**What's next: make it easy to get and easy to use.** Today a user has to build or download the binary, then hand-edit a different config file for each agent. The goal is two commands:

```sh
brew install prasenjit-net/tap/gitrs-mcp      # or: curl -fsSL <install script url> | sh
gitrs-mcp setup                                # registers it with the agents on this machine
```

Work items are checkboxes. Tick them when they ship, and note any deviation under the item, with the reason.

## Where we are

Facts checked against the repository and the agents' current documentation (October 2026).

- **Release pipeline: configured, nothing published.** cargo-dist builds five targets and generates a shell installer, a PowerShell installer and a Homebrew formula for `prasenjit-net/homebrew-tap`. The tap repository, its token, the crates.io publish and the first tag are still manual steps ([docs/development.md](docs/development.md#releasing)).
- **A curl installer already exists.** cargo-dist attaches `gitrs-mcp-installer.sh` to every release. It installs to `~/.cargo/bin`, can edit shell profiles, and can't run anything after installing.
- **Registration is manual.** [docs/configuration.md](docs/configuration.md) has copy-paste snippets for Claude Code, Claude Desktop, VS Code and Cursor.
- **The server exits outside a git repository.** `gitrs-mcp` run from a non-repository directory fails with `no git repository found at or above .`. A user-level registration with no `--repo` therefore only works when the client launches the server inside a repository. The server already handles MCP roots and has a "no repository is being served" error path, so allowing an empty start is a small change.
- **Agent configuration moves.** While checking formats for this plan: Windsurf's docs moved to the Devin docs and its config path changed, the Gemini CLI page says it was replaced by the Antigravity CLI on 2026-06-18, and VS Code's docs now label the `mcp.json` destinations deprecated in the guided flow (still supported). The design below assumes formats will drift.

## Decisions

My recommendation is in bold. Say if you disagree with any; the phases below assume the recommendations.

| # | Question | Recommendation and reason |
|---|---|---|
| D1 | `gitrs-mcp setup` or `gitrs-mcp --setup`? | **A subcommand, `setup`.** Existing registrations launch `gitrs-mcp --repo X`, so a bare invocation must stay "run the server". Setup also has its own flags (`--client`, `--remove`, `--dry-run`) that don't belong on the server command. |
| D2 | Edit each agent's config file ourselves, or call the agent's own CLI? | **Both.** Use the agent's CLI where one exists (`claude mcp add-json`, `codex mcp add`, `code --add-mcp`), because it owns the format and the scope rules and survives format changes. Edit files directly only where no CLI exists (Claude Desktop, Cursor). |
| D3 | Which scope by default? | **User scope** (every project). `--scope project` writes the files you commit (`.mcp.json`, `.cursor/mcp.json`, `.vscode/mcp.json`), and those entries must be portable, so they use the bare command `gitrs-mcp`, not an absolute path. |
| D4 | Does setup enable write tools? | **Never by default.** Only with `--allow-write`, or an explicit "yes" at the prompt, which defaults to "no" and says what the write tools can do. |
| D5 | The curl installer: ours, or cargo-dist's? | **Our own thin `install.sh`** that downloads the archive cargo-dist already builds, verifies its checksum, installs to `~/.local/bin`, and runs `gitrs-mcp setup`. A curl pipe can't chain setup with cargo-dist's installer, because the new binary isn't on `PATH` yet. The fallback, with no script to maintain, is to document `curl ... | sh` with cargo-dist's installer followed by `~/.cargo/bin/gitrs-mcp setup`. |
| D6 | Which agents first? | **Claude Code, Claude Desktop, Cursor, VS Code, Codex CLI.** Their formats are documented and checked below. Zed, Windsurf and Gemini CLI/Antigravity come later, after their formats are verified again. |
| D7 | When is 0.1.0 released? | **After `setup` and the installers**, so the first release already has the easy path. The release pipeline is rehearsed earlier with a release-candidate tag. |

## Phase 7: `gitrs-mcp setup`

### 7.0 Start without a repository

- [x] With no `--repo` given and the working directory outside any repository, start with no repository instead of exiting. Roots from the client add repositories as they do now. An explicit `--repo` that isn't a repository is still a startup error, since that is a mistake worth reporting.
- [x] Tools called with nothing served return an actionable error: open a folder in the client, or add `--repo <path>` to the registration. `list_repos` returns an empty list.
- [ ] Find out which of the five clients send roots (a one-time manual check) and record the result in docs/configuration.md. Clients that don't send roots need `--repo` in their registration, and setup must say so.
- Tests (done): integration tests in `tests/stdio.rs` that launch from a non-repository directory with and without roots. Compatible change; note it in the CHANGELOG.

### 7.1 Command surface

**Status: built.** Deviations from the design below, all deliberate:

- **Codex and VS Code are edited as files, not through their CLIs.** `codex mcp add` and `code --add-mcp` can't say whether an identical entry exists, have no dry run, and (for VS Code) have no documented removal. Editing `config.toml` (with `toml_edit`, keeping comments) and VS Code's user `mcp.json` can be compared, previewed, backed up and removed. Only Claude Code, whose file (`~/.claude.json`) is not meant to be edited by others, goes through its CLI, and for it an existing entry is reported as "exists" rather than compared (`--force` replaces it).
- **No automatic `${workspaceFolder}`.** Project-scope entries have no `--repo` unless you pass one; Cursor and VS Code start project servers in the project folder.
- **`--remove` and `--status` without `--scope`** look at both scopes; installing defaults to user.
- **Claude Desktop** has no `--repo` prompt: setup prints the advice and relies on `--repo`.
- **JSON files are rewritten with two-space indentation** (key order and other entries are kept). Files with comments are refused.
- **Extra files** in `src/setup/`: `env.rs`, `files.rs`, `cli.rs`, `document.rs`, `plan.rs`.

```
gitrs-mcp setup [--client <id>]... [--all] [--scope user|project]
                [--repo <path>]... [--allow-write] [--watch]
                [--name gitrs] [--dry-run] [--yes] [--force]
gitrs-mcp setup --status                    # what's registered where, and whether it still works
gitrs-mcp setup --remove [--client <id>]... # undo, using the same detection
gitrs-mcp setup --print --client <id>       # print the snippet for manual use; change nothing
```

- **Interactive** (a terminal and no `--client`): detect installed agents, show a checklist, ask about write tools, show exactly what will change, ask for confirmation.
- **Non-interactive** (no terminal): never prompt. Without `--client` or `--all`, fail with a message naming the flags.
- `--dry-run` shows the plan and touches nothing. `--yes` skips the confirmation. `--force` replaces an existing, different `gitrs` entry; without it, setup refuses and shows the difference.
- The server command is unchanged: `gitrs-mcp [--repo ..]` still runs the server, and every existing registration keeps working.

### 7.2 What gets registered

- **Name:** `gitrs`.
- **Command, user scope:** an absolute path. GUI apps (Claude Desktop, Cursor and VS Code launched from the Dock) don't inherit the shell's `PATH`, so `~/.cargo/bin` or `~/.local/bin` often isn't visible to them.
  - Don't resolve symlinks. Homebrew's `/opt/homebrew/bin/gitrs-mcp` survives `brew upgrade`; the `Cellar/<version>/` path it points to doesn't. If the running binary is inside a `Cellar/`, register the `bin` path instead.
- **Command, project scope:** the bare `gitrs-mcp`, so the committed file works on every teammate's machine. Claude Code asks for approval before using a project-scoped server, and setup says so.
- **Arguments:** none by default at user scope (the client's working directory and roots pick the repository). `--repo` values, `--allow-write` and `--watch` are added when asked for. Project scope uses `${workspaceFolder}` where the client supports it (VS Code, Cursor).
- Claude Desktop starts servers with an unknown working directory, so setup asks for a `--repo` unless 7.0 found that it sends roots.

### 7.3 Agents

One entry per agent in a data-driven registry. Adding an agent means adding an entry and its test fixtures; nothing else changes. Each entry records the date and client version its format was last verified.

| Agent | Detected by | Registered with | Scopes |
|---|---|---|---|
| Claude Code (`claude-code`) | `claude` on `PATH` | `claude mcp add-json gitrs '<json>' --scope user\|project\|local`; check with `claude mcp get gitrs`, undo with `claude mcp remove gitrs` | user, project (`.mcp.json`), local |
| Claude Desktop (`claude-desktop`) | its config directory exists | edit `claude_desktop_config.json`, key `mcpServers`: `~/Library/Application Support/Claude/` on macOS, `%APPDATA%\Claude\` on Windows. Needs a full restart. No Linux path is documented upstream, so Linux isn't supported yet | user |
| Cursor (`cursor`) | `cursor` on `PATH`, or `~/.cursor` exists | edit `~/.cursor/mcp.json` or `.cursor/mcp.json`, key `mcpServers`, with `"type": "stdio"` | user, project |
| VS Code (`vscode`) | `code` on `PATH` (often missing on macOS: say how to install it) | user: `code --add-mcp '<json>'`. Project: edit `.vscode/mcp.json`, key `servers`, with `"type": "stdio"` | user, project |
| Codex CLI (`codex`) | `codex` on `PATH` | `codex mcp add gitrs -- <command> <args>`, or edit `~/.codex/config.toml` (`[mcp_servers.gitrs]`) with a comment-preserving TOML editor. A project `.codex/config.toml` only applies in trusted projects | user, project |

Later, after re-verifying: **Zed** (`context_servers` in `settings.json`), **Windsurf** (`mcpServers` in `~/.config/devin/mcp_config.json`, legacy Cascade agent only), **Gemini CLI or its successor** (`gemini mcp add -s user ...`; its default scope is project, not user).

To check before building each one:
- Whether removal has a CLI (`codex mcp remove`, VS Code's user config has no documented path). Where there isn't one, `--remove` prints the manual step instead of guessing a file.
- Whether the agent's CLI on Windows is a `.cmd` shim, which Rust's `Command` doesn't find by bare name.

### 7.4 Editing other tools' files safely

setup changes files that belong to other applications, so it follows the same discipline as the write tools.

- Parse, don't pattern-match. Change only this server's entry; keep every other entry, key order and (for TOML) comments.
- A file with comments in a format we can't rewrite faithfully (JSONC): refuse, print the snippet, change nothing.
- A missing file is created, with its parent directories. An invalid file is never overwritten: report the parse error and stop.
- An identical entry is "already registered" and nothing is written. A different entry needs `--force`.
- Back up the file before the first change in a run (`<file>.gitrs-backup-<timestamp>`) and print the backup path.
- Write atomically (temp file in the same directory, then rename) and keep the file's permissions, since these files can hold other servers' secrets.
- Entries contain only the command and arguments. No secrets, no environment values.
- Subprocesses are limited to the agents' own CLIs, run with a fixed argument list (never through a shell) and a timeout. No network.
- `--dry-run` and every refusal leave all files byte for byte unchanged.

### 7.5 Code layout

Per [CLAUDE.md](CLAUDE.md): `main.rs` keeps CLI and transport only; setup is its own layer and imports neither `rmcp` nor the `tools` or `server` modules.

```
src/setup/
├─ mod.rs        plan, confirm, apply: the orchestration; no agent specifics
├─ agents.rs     the registry: one entry per agent (detection, how to register, how to remove)
├─ binary.rs     which path to register (see 7.2)
├─ json.rs       order-preserving JSON edits
├─ toml.rs       comment-preserving TOML edits
└─ prompt.rs     the terminal checklist and confirmations
```

Every filesystem location comes from an injectable environment (home, `APPDATA`, `XDG_CONFIG_HOME`, `PATH`), so tests never touch the real home directory. A comment-preserving TOML editor (`toml_edit`) is the only new dependency expected; the prompt can be hand-written. Keep the public surface `pub(crate)` except what `main.rs` calls.

### 7.6 Tests

- **Per agent**, golden files for: no config yet; other servers present and kept; already registered (no write, bytes identical); a different entry (refused without `--force`); invalid file (refused, unchanged); comments and key order kept.
- **Fake agent CLIs**: scripts on a temporary `PATH` that record their arguments, asserting the exact command lines for `claude`, `codex` and `code`. Unix only; Windows is covered by `cargo check --target x86_64-pc-windows-gnu`, as now.
- **Idempotence and undo**: running setup twice changes nothing the second time; `--remove` restores the original bytes when setup created the entry.
- **Dry run** leaves a snapshot of the whole fake home unchanged (the existing `snapshot` helper).
- **End to end**: run the built binary with `HOME` pointed at a temporary directory, `setup --client cursor --yes`, then launch the command from the file it wrote through the stdio harness and check that the handshake lists the tools. This proves what was registered actually starts.
- **`--status`** reports a registration whose binary no longer exists (for example after `brew uninstall`).
- `make check` and `make coverage` stay green; the new module should be near full coverage.
- **Manual matrix** before each release, on the real applications (Claude Desktop, Cursor, VS Code, Codex, Claude Code): run setup, restart, call a tool, run `--remove`. Formats drift, and only the real clients can confirm them. Record the result and versions in the agent's registry entry.

### 7.7 Documentation

- [x] README: a quick start with the two commands.
- [x] docs/configuration.md: the `setup` reference (the roots check is still open, see 7.0), and the manual snippets kept as a fallback for every agent.
- [x] docs/security.md: which files setup touches, backups, and that write tools stay off unless asked for.
- [x] docs/architecture.md: the setup layer. docs/development.md: how to add an agent.
- [x] CHANGELOG. Examples in the docs must be real output, taken from a run against a temporary home.

## Phase 8: Distribution

### 8.1 Rehearse the release and publish 0.1.0

Needs you; see [Manual steps](#manual-steps-for-you).

- [ ] Before the first tag, run `dist plan` and `dist build` for the host target. Neither exercises the publish job or the token.
- [ ] Rehearse with a tag such as `v0.1.0-rc.1`. First check how cargo-dist treats pre-release tags (`dist plan --tag ...`): it may skip the Homebrew job for them. Delete the rehearsal tag and release afterwards.
- [ ] Record the exact release asset names (archive, `.sha256`, `sha256.sum`) that 8.3 depends on.
- [x] Decide the archive format: done, `.tar.gz` (`unix-archive` in `dist-workspace.toml`). cargo-dist's default for Unix is `.tar.xz`; minimal containers often lack `xz`, so `.tar.gz` may be safer (look for the matching dist option).
- [ ] Linux builds are glibc only, so Alpine and static-binary containers aren't supported. Evaluate adding `*-unknown-linux-musl` targets (gix pulls in some C code, so check the musl build first).
- [x] Tag `v0.1.0` (released 2026-10-28; Homebrew formula published, `brew install` verified). `cargo publish` runs from the *Publish crate* workflow once `CARGO_REGISTRY_TOKEN` is set. README notes updated.

### 8.2 Homebrew

The formula installs the prebuilt binary and covers macOS and Linux on Intel and ARM, matching the build targets.

- [ ] After the first release: `brew install prasenjit-net/tap/gitrs-mcp` works on a clean macOS and a clean Linuxbrew.
- [ ] Add a caveat to the formula: "Run `gitrs-mcp setup` to register with your agents". Check whether cargo-dist 0.32 can customise the formula. If not, try post-processing in the tap's own workflow, and only then settle for documenting it.
- [ ] Tap CI: `brew audit --strict`, install and `brew test` on macOS and Linux, on every formula change.
- [ ] `brew upgrade` must not break registrations: they point at the stable `bin` path (7.2), and a test covers choosing it over a `Cellar/` path.
- [ ] `brew uninstall` can't run a cleanup hook, so registrations are left pointing at nothing. Document `gitrs-mcp setup --remove` before uninstalling, and have `--status` flag dangling entries.
- Out of scope for now: submitting to Homebrew core (it needs a build-from-source formula and enough notability).

### 8.3 `install.sh`

**Status: built and tested against a fixture release on macOS and Debian (dash).** Extras: `--print-target` (used by the tests). `--uninstall` offers `setup --remove` first. Under `curl | sh` the script reads answers from `/dev/tty`.

```sh
curl -fsSL https://raw.githubusercontent.com/prasenjit-net/gitrs-mcp/master/install.sh | sh
curl -fsSL .../install.sh | sh -s -- --version v0.1.0 --no-setup
```

- [x] **Where:** `install.sh` at the repository root, so the URL is short. It only ever downloads release assets, so serving it from `master` is safe.
- [x] **Behaviour:**
  - POSIX `sh`, not bash, so it runs under dash and Alpine's ash.
  - Detect OS and architecture to a cargo-dist target; recognise an Apple Silicon shell running under Rosetta (`sysctl -n sysctl.proc_translated`); fail clearly on unsupported systems, with a pointer to `cargo install`.
  - Download the archive for `latest` (the `releases/latest/download/` redirect, which has no API rate limit) or for `--version`, plus its checksum; verify with `sha256sum` or `shasum -a 256`; stop on a mismatch.
  - Install to `$GITRS_INSTALL_DIR` or `~/.local/bin`. Never use `sudo`. If the directory isn't on `PATH`, print the exact line to add; don't edit shell profiles unprompted.
  - Unless `--no-setup`, run `<dir>/gitrs-mcp setup`. Under `curl | sh`, stdin is the script itself, so the prompts must read from `/dev/tty`; without one, print `Run: gitrs-mcp setup` instead.
  - Flags: `--version`, `--dir`, `--no-setup`, `--dry-run` (download nothing, write nothing), `--uninstall` (remove the binary and remind about `setup --remove`).
- [x] **Safe to pipe into a shell:** all work inside a function called on the last line, so a truncated download can't run half a script; `set -eu`; `curl --proto '=https' --tlsv1.2`; a temporary directory removed on exit; no `eval`. Short enough to read before running, with a header comment saying what it does.
- [x] **Tests:** (the post-release workflow exists but has not run against a real release yet)
  - ShellCheck in CI.
  - A CI job that runs the script against a local fixture release directory (`GITRS_RELEASE_BASE_URL=file://...`, which curl supports) on Ubuntu and macOS: successful install, checksum mismatch fails, unsupported architecture fails, `PATH` warning, reinstall is idempotent, `--dry-run` writes nothing.
  - After each release, a workflow runs the real `curl | sh` on clean Ubuntu and macOS runners and calls `gitrs-mcp --version`.
- Windows keeps cargo-dist's PowerShell installer. A custom `.ps1` isn't planned.
- If D5 goes the other way, cargo-dist's `gitrs-mcp-installer.sh` stays as the only script and this section shrinks to documentation.

### 8.4 Later, optional

- `cargo binstall` metadata, so `cargo binstall gitrs-mcp` fetches the prebuilt binary.
- Windows package managers (Scoop, winget).
- A Claude Desktop one-click bundle (`.mcpb`) and "Add to Cursor" / "Install in VS Code" links in the README. All three formats need verifying first.
- Zed, Windsurf and Gemini CLI/Antigravity in the setup registry (7.3).

## Order of work

1. **7.0** start without a repository. Small, independent, and needed for user-level registration.
2. **7.1 to 7.6**, beginning with Claude Code and Cursor (one agent with a CLI, one with a file), then the other three. Docs (7.7) with each agent.
3. **8.3** `install.sh`. Needs `setup` for chaining, but is testable against fixtures without a release.
4. **8.1** release candidate, which needs the manual steps below. Fix whatever it shows.
5. **8.2** and the real `v0.1.0`, then the post-release smoke workflow.

## Manual steps for you

- Create the `prasenjit-net/homebrew-tap` repository, and add a `HOMEBREW_TAP_TOKEN` secret to gitrs-mcp: a token that can push to the tap only.
- Run `cargo login` with a crates.io token; tag and publish when the rehearsal is clean.
- Run the manual matrix in 7.6 on the real Claude Desktop, Cursor, VS Code, Codex and Claude Code. The GUI applications can't be driven from here.

## Risks

| Risk | Mitigation |
|---|---|
| An agent changes its config format or location. | Data-driven registry with a verified date per entry; the agent's own CLI where there is one; `--print` as a fallback; the manual matrix before each release. |
| setup damages a user's config file. | The rules in 7.4: parse, don't overwrite, refuse on doubt, back up, write atomically, dry run. |
| A `curl \| sh` installer is a trust ask. | HTTPS only, checksum verification, no `sudo`, a script short enough to read, and a documented non-script route (Homebrew, `cargo install`, downloading the archive). |
| GUI apps can't find the binary. | Register an absolute path at user scope; keep the Homebrew `bin` path. |
| Not every client sends roots. | 7.0 records which do; setup asks for `--repo` for those that don't. |
| cargo-dist can't customise the Homebrew formula. | Post-process in the tap, or document the caveat. |

## Done when

- A clean macOS and a clean Linux machine each go from nothing to a working registration in Claude Code with the two commands above, through both Homebrew and `install.sh`.
- `gitrs-mcp setup`, `--status` and `--remove` work for all five agents, and the manual matrix has passed on the real applications.
- `make check` and `make coverage` pass, the docs are updated, and the CHANGELOG lists the additions.
