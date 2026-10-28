use std::path::Path;

use anyhow::{Context, bail};
use gix::{
    dir::{
        entry::{Kind as DiskKind, Status as DirStatus},
        walk::EmissionMode,
    },
    status::{UntrackedFiles, index_worktree},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    pathspec_patterns,
    write::{ensure_idle, require_worktree},
};

/// Paths listed per call; `count` always has the full number.
const MAX_LISTED: usize = 500;

fn default_true() -> bool {
    true
}

/// Which ignored files `clean` removes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IgnoredFiles {
    /// Leave ignored files alone (the default, like `git clean`).
    #[default]
    Keep,
    /// Remove ignored files too, like `git clean -x` (build output, logs, ...).
    Also,
    /// Remove only ignored files, like `git clean -X`.
    Only,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CleanArgs {
    /// Limit to these paths or pathspec patterns (e.g. `build/`, `*.tmp`), relative to the repository
    /// root. Omit or pass `[]` for the whole working tree.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Also remove untracked directories, like `git clean -d` (default false: only files directly in
    /// tracked directories are removed).
    #[serde(default)]
    pub directories: bool,
    /// Whether ignored files go too: `keep` (default), `also` (`-x`) or `only` (`-X`).
    #[serde(default)]
    pub ignored: IgnoredFiles,
    /// Only list what would be removed (default true). Set `false` to delete.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CleanedPath {
    /// Path relative to the repository root; directories end in `/`.
    pub path: String,
    /// True for a whole directory (removed with everything in it).
    pub directory: bool,
    /// True when the path is ignored rather than merely untracked.
    pub ignored: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CleanResult {
    /// What is (or would be) removed, ordered by path; at most 500.
    pub removed: Vec<CleanedPath>,
    /// Number of paths removed, including any beyond the listed 500.
    pub count: usize,
    /// Whether this was a dry run; if so, nothing was deleted.
    pub dry_run: bool,
}

/// Delete untracked (and optionally ignored) files from the working tree, like `git clean -f`.
/// Nested repositories are never removed. Dry run by default.
pub fn clean(repo: &gix::Repository, args: CleanArgs) -> anyhow::Result<CleanResult> {
    let workdir = require_worktree(repo, "clean")?.to_owned();
    ensure_idle(repo, "clean")?;
    let mut removed = candidates(repo, &args)?;
    removed.sort_by(|a, b| a.path.cmp(&b.path));
    if !args.dry_run {
        for entry in &removed {
            delete(&workdir, entry)?;
        }
    }
    let count = removed.len();
    removed.truncate(MAX_LISTED);
    Ok(CleanResult {
        removed,
        count,
        dry_run: args.dry_run,
    })
}

fn candidates(repo: &gix::Repository, args: &CleanArgs) -> anyhow::Result<Vec<CleanedPath>> {
    let want_ignored = args.ignored != IgnoredFiles::Keep;
    let status = repo
        .status(gix::progress::Discard)?
        .untracked_files(UntrackedFiles::Collapsed)
        .dirwalk_options(|opts| {
            opts.emit_ignored(want_ignored.then_some(EmissionMode::CollapseDirectory))
        })
        .into_index_worktree_iter(pathspec_patterns(&args.paths))?;
    let mut found = Vec::new();
    for item in status {
        let index_worktree::Item::DirectoryContents { entry, .. } = item? else {
            continue;
        };
        let ignored = match entry.status {
            DirStatus::Untracked => false,
            DirStatus::Ignored(_) => true,
            _ => continue,
        };
        let keep = match args.ignored {
            IgnoredFiles::Keep => ignored,
            IgnoredFiles::Also => false,
            IgnoredFiles::Only => !ignored,
        };
        let directory = match entry.disk_kind {
            // Another repository's files are never ours to delete, as with `git clean` without `-ff`.
            // Sockets, FIFOs and the like aren't ours to delete either.
            Some(DiskKind::Repository | DiskKind::Untrackable) | None => continue,
            Some(DiskKind::Directory) => true,
            Some(DiskKind::File | DiskKind::Symlink) => false,
        };
        if keep || (directory && !args.directories) {
            continue;
        }
        let mut path = entry.rela_path.to_string();
        if directory {
            path.push('/');
        }
        found.push(CleanedPath {
            path,
            directory,
            ignored,
        });
    }
    Ok(found)
}

fn delete(workdir: &Path, entry: &CleanedPath) -> anyhow::Result<()> {
    let rela = entry.path.trim_end_matches('/');
    if rela.is_empty() || rela.split('/').next() == Some(".git") {
        bail!("refusing to delete `{}`", entry.path);
    }
    let full = workdir.join(rela);
    let result = if entry.directory {
        // `remove_dir_all` deletes symlinks inside the directory without following them.
        std::fs::remove_dir_all(&full)
    } else {
        std::fs::remove_file(&full)
    };
    result.with_context(|| format!("cannot delete `{}`", entry.path))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::testutil::{
        assert_unchanged, bare_repo, fixture_repo, git_stdout, run_git, snapshot, twins,
        worktree_files, write,
    };

    fn args(directories: bool, ignored: IgnoredFiles, paths: &[&str], dry_run: bool) -> CleanArgs {
        CleanArgs {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            directories,
            ignored,
            dry_run,
        }
    }

    /// Untracked files at the top and in a tracked directory, an untracked directory, ignored files
    /// and an ignored directory, and a nested repository.
    fn messy(root: &Path) {
        write(root, ".gitignore", "*.log\ntarget/\n");
        write(root, "src/lib.rs", "tracked\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "layout"]);
        write(root, "scratch.txt", "untracked\n");
        write(root, "src/tmp.rs", "untracked in a tracked dir\n");
        write(root, "notes/a.md", "untracked dir\n");
        write(root, "debug.log", "ignored\n");
        write(root, "target/out.bin", "ignored dir\n");
        std::fs::create_dir(root.join("nested")).unwrap();
        run_git(&root.join("nested"), &["init", "-q"]);
        write(&root.join("nested"), "inner.txt", "kept\n");
    }

    /// The paths `git clean -n` would remove.
    fn git_clean(root: &Path, flags: &[&str]) -> Vec<String> {
        let mut cmd = vec!["clean", "-n"];
        cmd.extend_from_slice(flags);
        git_stdout(root, &cmd)
            .lines()
            .filter_map(|l| l.strip_prefix("Would remove "))
            .map(str::to_owned)
            .collect()
    }

    fn paths(res: &CleanResult) -> Vec<String> {
        res.removed.iter().map(|r| r.path.clone()).collect()
    }

    #[test]
    fn dry_runs_match_git_clean() {
        let [(dir, repo), _] = twins(messy);
        let root = dir.path();
        let cases: [(bool, IgnoredFiles, &[&str], &[&str]); 6] = [
            (false, IgnoredFiles::Keep, &[], &[]),
            (true, IgnoredFiles::Keep, &[], &["-d"]),
            (true, IgnoredFiles::Also, &[], &["-d", "-x"]),
            (false, IgnoredFiles::Also, &[], &["-x"]),
            (true, IgnoredFiles::Only, &[], &["-d", "-X"]),
            (true, IgnoredFiles::Keep, &["src/"], &["-d", "--", "src/"]),
        ];
        let before = snapshot(root);
        for (directories, ignored, pathspec, flags) in cases {
            let res = clean(&repo, args(directories, ignored, pathspec, true)).unwrap();
            let mut ours = paths(&res);
            let mut git = git_clean(root, flags);
            ours.sort();
            git.sort();
            assert_eq!(ours, git, "{flags:?}");
            assert!(res.dry_run);
        }
        assert_unchanged(root, &before);
        let parsed: CleanArgs = serde_json::from_str("{}").unwrap();
        assert!(parsed.dry_run && !parsed.directories && parsed.ignored == IgnoredFiles::Keep);
    }

    #[test]
    fn cleaning_matches_git_clean_force() {
        let [(ours, repo), (theirs, _)] = twins(messy);
        let res = clean(&repo, args(true, IgnoredFiles::Also, &[], false)).unwrap();
        run_git(theirs.path(), &["clean", "-q", "-f", "-d", "-x"]);
        assert_eq!(worktree_files(ours.path()), worktree_files(theirs.path()));
        assert!(
            ours.path().join("nested/inner.txt").exists(),
            "nested repositories are kept"
        );
        assert_eq!(res.count, 5);
        let target = res.removed.iter().find(|r| r.path == "target/").unwrap();
        assert!(target.directory && target.ignored);
    }

    #[test]
    fn bare_repositories_are_refused() {
        let (_dir, bare) = bare_repo();
        let err = clean(&bare, args(false, IgnoredFiles::Keep, &[], true)).unwrap_err();
        assert!(err.to_string().contains("bare"));
        let (dir, repo) = fixture_repo();
        let res = clean(&repo, args(true, IgnoredFiles::Also, &[], false)).unwrap();
        assert_eq!(res.count, 0);
        assert!(dir.path().join("a.txt").exists());
    }
}
