# Security

gitrs-mcp gives an AI model access to Git repositories on your machine. This page describes what the model can and can't reach through it, and which tests enforce each guarantee. It's the outcome of the security review done before the first release.

## Threat model

The server trusts the person who starts it and the MCP client that sends requests. It does not trust:

- **The model's arguments.** Paths, revisions, URIs and repository names can be anything, including attempts to escape the repository.
- **Repository content.** A cloned repository can contain hostile trees, symlinks, file names and attributes.
- **Repositories owned by someone else.** Their configuration can name programs to run.

## Guarantees

### Nothing goes over the network

gix is compiled without any network transport, so fetch, push and clone don't exist in the binary. Remotes are only ever read from local configuration.

### Read-only unless you say otherwise

Without `--allow-write`, write tools aren't registered and every tool is annotated read-only. A test checks that the write tools are listed only with the flag (`server::tests::write_tools_are_registered_only_with_allow_write`). Destructive tools default to a dry run. Every refusal leaves the repository byte for byte unchanged, and each write tool has a test that snapshots the whole repository around a refused call.

### Paths stay inside the repository

- **Path arguments** go through `tools::repo_path`, which refuses absolute paths and `..` (`repo_path_normalizes_and_rejects_escapes`).
- **Resource URIs** are percent-decoded before those checks, so `%2E%2E` and `%2F` are refused too (`malformed_uris_say_what_is_expected`).
- **Reading the working tree** (`read_file` with `rev: "worktree"`, `grep`, file resources):
  - A path with a `.git` component anywhere is refused, whatever its case and with trailing dots or spaces, because case-insensitive filesystems (macOS, Windows) and NTFS resolve those to the real `.git`. See `git_directories_are_refused_whatever_their_spelling`.
  - A path whose parent directory resolves outside the repository through a symlink is refused.
  - A symlink is reported by its target and never followed.
- **Writing the working tree** (`restore`, `switch`, `reset --hard`, `merge`, `stash_pop`, ...):
  - Files go through gix's checkout, which validates every path component with git's rules (`core.protectHFS`, `core.protectNTFS`). A hostile commit whose tree contains `.GIT/config` (CVE-2014-9390) is refused before anything is written (`trees_that_write_into_git_are_refused`).
  - A path whose nearest existing directory leads outside the working tree through a symlink is never written (`writes_through_symlinked_directories_are_refused`).
- **`core.worktree`** is honoured, as git does: the configured directory is the working tree, and the same checks apply inside it (`core_worktree_moves_the_working_tree`).
- **Filesystem paths given relative to the repository** (`worktree_add`, `worktree_remove`) are joined to the repository root, never to the process directory (`relative_paths_are_from_the_repository_root`). `worktree_add` can create a worktree anywhere you could with `git worktree add`, but only with `--allow-write`.

### Only the repositories you serve

- **Choosing a repository.** The `repo` argument can only name a served repository, by its name or by an absolute path inside it. Relative paths and paths containing `..` are refused (`repositories_are_named_and_selected`).
- **Client roots.** A repository at one of the client's MCP roots is served only if its root lies inside the declared directory. A root inside a larger repository doesn't expose the rest of it (`client_roots_serve_repositories_inside_them_only`).

### Repositories owned by someone else

gix applies git's ownership rule, `safe.directory`. A repository the current user doesn't own, and that isn't listed in `safe.directory`, opens with reduced trust, and its repository-local configuration is ignored. Programs it names (filters, merge drivers, `core.hooksPath`) therefore never run. git refuses to work in such a repository at all. gitrs-mcp still allows reads, but refuses every write tool with a message saying how to trust the repository. `repo_info` reports `trusted: false` for such a repository.

The tests are `write_tools_refuse_untrusted_repositories`, `every_write_wrapper_checks_trust` and `repositories_owned_by_others_are_untrusted`.

In repositories you trust, configuration works as it does for git. Clean and smudge filters and merge drivers from your configuration and `.gitattributes` run during staging, checkouts and merges, as they would with `git add`, `git checkout` or `git merge`. Hooks never run: `commit` reports the hooks it skipped instead.

### Secrets stay out of output

`config_get` and `list_submodules` replace values that may be secrets with `<redacted>` and set `redacted: true`:

- anything under `credential`;
- keys named like a password, token or secret;
- `http.extraHeader`, including `http.<url>.extraHeader`;
- the password of any URL, including a URL inside a subsection name such as `url.https://user:pass@host/.insteadOf`.

Configuration that gix derives from environment variables isn't listed at all. The tests are `secrets_are_redacted` and `urls_are_redacted_and_kept_relative`.

### Bounded work and output

- **Output.** Every list is paginated, and every content field has a byte budget.
- **Long calls.** Long walks report progress and stop when the client cancels. `blame` is the exception, since gix's blame can't be interrupted.
- **Regexes.** They use Rust's `regex` crate, which runs in linear time, so a hostile pattern can't cause catastrophic backtracking.

## Known limits

- **Races.** A path is checked, then read or written. Another process changing the working tree between the two steps (time of check to time of use) could defeat the symlink checks. gitrs-mcp assumes nothing else is maliciously rewriting your working tree while it runs.
- **Environment variables.** gix honours `GIT_DIR`, `GIT_WORK_TREE` and related variables, as git does. They come from whoever starts the server, which the threat model trusts.
- **Trusted configuration.** In a repository you trust, configured filters and merge drivers run, exactly as with git.

## Reporting a vulnerability

Please report security issues privately through GitHub's "Report a vulnerability" button on the repository's Security tab, rather than in a public issue.
