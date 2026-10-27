//! Guards and helpers shared by the tools that change the repository.
//!
//! Those tools are only registered with `--allow-write`. Each checks everything it can before
//! writing anything, so a refused call leaves the repository exactly as it was.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use gix::{
    bstr::BStr,
    refs::{FullName, FullNameRef},
};

use super::in_progress;

const INDEX_LOCKED: &str = "the index is locked (`.git/index.lock` exists): another git process is \
                            using it; retry when it has finished, or delete the lock file if no \
                            git process is running";
const NO_IDENTITY: &str = "no committer identity is configured; set `user.name` and `user.email` \
                           in the repository's git config (or globally), then retry";

/// Refuse while a merge, rebase, cherry-pick, revert or bisect is in progress, so a write can't
/// tangle with the state that operation keeps.
pub fn ensure_idle(repo: &gix::Repository, action: &str) -> anyhow::Result<()> {
    if let Some(operation) = in_progress(repo) {
        bail!(
            "cannot {action} while an operation is in progress ({operation}); finish or abort it \
             with git first (`conflicts` shows what is left to resolve)"
        );
    }
    Ok(())
}

/// The working tree root, or an error explaining that a bare repository has none.
pub fn require_worktree<'repo>(
    repo: &'repo gix::Repository,
    action: &str,
) -> anyhow::Result<&'repo Path> {
    repo.workdir().with_context(|| {
        format!("cannot {action}: this repository is bare and has no working tree")
    })
}

/// Read-modify-write the index the way git does: take `index.lock` first so a concurrent `git add`
/// can't be lost, apply `edit`, then write and release. If `edit` fails, the lock is dropped and the
/// index stays as it was.
///
/// Only the end-of-index-entry extension is written back. The cache-tree would be stale after an
/// edit (gix writes it unchanged), and git rebuilds the untracked cache and fsmonitor data itself.
pub fn update_index<T>(
    repo: &gix::Repository,
    edit: impl FnOnce(&mut gix::index::State) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = repo.index_path();
    let lock = gix::lock::File::acquire_to_update_resource(
        &path,
        gix::lock::acquire::Fail::Immediately,
        None,
        0,
    )
    .context(INDEX_LOCKED)?;
    let mut index = if path.is_file() {
        gix::index::File::at(&path, repo.object_hash(), false, Default::default())?
    } else {
        gix::index::File::from_state(gix::index::State::new(repo.object_hash()), path)
    };
    // A split index was already merged with its shared index when read, so writing it back whole
    // keeps every entry. A sparse index has directory entries the write tools don't handle.
    if index.is_sparse() {
        bail!(
            "this repository uses a sparse index, which gitrs-mcp can't write; turn it off with \
             `git sparse-checkout disable`, or use git"
        );
    }
    let out = edit(&mut index)?;
    index.sort_entries();
    let options = gix::index::write::Options {
        extensions: gix::index::write::Extensions::Given {
            tree_cache: false,
            end_of_index_entry: true,
        },
        skip_hash: false,
    };
    let mut writer = std::io::BufWriter::new(lock);
    index.write_to(&mut writer, options)?;
    writer
        .into_inner()
        .map_err(|err| err.into_error())?
        .commit()
        .map_err(|err| err.error)?;
    Ok(out)
}

/// Hold `.git/index.lock` without writing the index, so it can't change while it's read, as
/// `git commit` does. The lock goes away when the marker is dropped.
pub fn hold_index_lock(repo: &gix::Repository) -> anyhow::Result<gix::lock::Marker> {
    gix::lock::Marker::acquire_to_hold_resource(
        repo.index_path(),
        gix::lock::acquire::Fail::Immediately,
        None,
        0,
    )
    .context(INDEX_LOCKED)
}

/// The identity to record as committer, and in reflogs.
pub fn committer(repo: &gix::Repository) -> anyhow::Result<gix::actor::Signature> {
    let signature = repo.committer().context(NO_IDENTITY)??;
    Ok(signature.to_owned()?)
}

/// gix can't sign commits or tags, so when the repository asks for signatures (`key` is
/// `commit.gpgSign` or `tag.gpgSign`) refuse rather than create an unsigned `what`.
pub fn refuse_signing(repo: &gix::Repository, key: &str, what: &str) -> anyhow::Result<()> {
    if repo.config_snapshot().boolean(key) == Some(true) {
        bail!(
            "`{key}` is enabled, but gitrs-mcp can't sign {what}; use git instead, or turn signing off \
             for this repository"
        );
    }
    Ok(())
}

/// `refs/heads/<name>` for a branch given as `name` (or already as `refs/heads/name`), validated
/// like `git check-ref-format --branch`.
pub fn branch_ref(name: &str) -> anyhow::Result<FullName> {
    let short = name.strip_prefix("refs/heads/").unwrap_or(name);
    // gix accepts a leading `-`, but git refuses it because such a name reads like an option.
    if short.starts_with('-') {
        bail!("`{name}` isn't a valid branch name: it must not start with `-`");
    }
    let full = format!("refs/heads/{short}");
    gix::validate::reference::branch_name(BStr::new(&full)).map_err(|err| {
        anyhow::anyhow!(
            "`{name}` isn't a valid branch name ({err}); use letters, digits, `-`, `_`, `.` and `/`, \
             without spaces, `..`, `~`, `^`, `:` or a trailing `.lock`"
        )
    })?;
    Ok(FullName::try_from(full)?)
}

/// `refs/tags/<name>` for a tag given as `name` (or as `refs/tags/name`), validated like git does.
pub fn tag_ref(name: &str) -> anyhow::Result<FullName> {
    let short = name.strip_prefix("refs/tags/").unwrap_or(name);
    let full = format!("refs/tags/{short}");
    gix::validate::reference::name(BStr::new(&full)).map_err(|err| {
        anyhow::anyhow!(
            "`{name}` isn't a valid tag name ({err}); use letters, digits, `-`, `_`, `.` and `/`, \
             without spaces, `..`, `~`, `^` or `:`"
        )
    })?;
    Ok(FullName::try_from(full)?)
}

/// A checkout where `branch` is checked out: the main worktree or a linked one. With
/// `skip_current`, the worktree this server serves doesn't count.
pub fn checked_out_at(
    repo: &gix::Repository,
    branch: &FullNameRef,
    skip_current: bool,
) -> anyhow::Result<Option<PathBuf>> {
    let current = canonical(repo.git_dir());
    for worktree in repo.worktrees_including_main()? {
        let worktree = worktree?;
        if skip_current && canonical(worktree.git_dir()) == current {
            continue;
        }
        if worktree.head_name()?.as_ref().map(AsRef::as_ref) == Some(branch) {
            let path = worktree.workdir().unwrap_or_else(|| worktree.git_dir());
            return Ok(Some(path.to_owned()));
        }
    }
    Ok(None)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

/// Tidy a message the way `git commit -m` and `git tag -m` do (`--cleanup=whitespace`): strip
/// trailing whitespace from every line, drop leading and trailing blank lines, collapse runs of blank
/// lines into one, and end with a newline. `None` when nothing is left.
pub fn clean_message(message: &str) -> Option<String> {
    let mut out = String::new();
    let mut blank_run = false;
    for line in message.lines().map(str::trim_end) {
        if line.is_empty() {
            blank_run = !out.is_empty();
            continue;
        }
        if blank_run {
            out.push('\n');
            blank_run = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{fixture_repo, open, run_git, try_git, write};

    #[test]
    fn clean_message_matches_git_whitespace_cleanup() {
        assert_eq!(clean_message("subject").as_deref(), Some("subject\n"));
        assert_eq!(
            clean_message("\n\n  subject  \n\n\n\nbody \t\nmore\n\n").as_deref(),
            Some("  subject\n\nbody\nmore\n")
        );
        assert_eq!(clean_message(" \n\t\n"), None);
    }

    #[test]
    fn names_are_validated() {
        assert_eq!(
            branch_ref("feature/x").unwrap().as_bstr(),
            "refs/heads/feature/x"
        );
        assert_eq!(
            branch_ref("refs/heads/main").unwrap().as_bstr(),
            "refs/heads/main"
        );
        for bad in ["a b", "a..b", "x.lock", "HEAD", "-start", "a~1", ""] {
            assert!(branch_ref(bad).is_err(), "{bad}");
        }
        assert_eq!(tag_ref("v1.0").unwrap().as_bstr(), "refs/tags/v1.0");
        assert!(tag_ref("v 1").is_err());
    }

    #[test]
    fn guards() {
        let (dir, repo) = fixture_repo();
        assert!(ensure_idle(&repo, "commit").is_ok());
        assert_eq!(committer(&repo).unwrap().email, "test@example.com");
        let main = branch_ref("main").unwrap();
        assert!(
            checked_out_at(&repo, main.as_ref(), false)
                .unwrap()
                .is_some()
        );
        assert!(
            checked_out_at(&repo, main.as_ref(), true)
                .unwrap()
                .is_none()
        );
        let feature = branch_ref("feature").unwrap();
        assert!(
            checked_out_at(&repo, feature.as_ref(), false)
                .unwrap()
                .is_none()
        );

        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        write(root, "a.txt", "theirs\n");
        run_git(root, &["commit", "-q", "-am", "theirs"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "ours\n");
        run_git(root, &["commit", "-q", "-am", "ours"]);
        assert!(!try_git(root, &["merge", "-q", "feature"]));
        let err = ensure_idle(&open(root), "commit").unwrap_err().to_string();
        assert!(err.contains("Merge"), "{err}");
    }
}
