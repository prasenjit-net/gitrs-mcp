use gix::{
    ObjectId,
    bstr::{BStr, ByteSlice},
    index::entry::{Mode, Stage},
    submodule::config::Branch,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{config::redact_url_password, page_limit, paginate};

const DEFAULT_SUBMODULE_LIMIT: usize = 100;
const MAX_SUBMODULE_LIMIT: usize = 1000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSubmodulesArgs {
    /// Maximum submodules to return (default 100, max 1000).
    pub limit: Option<usize>,
    /// Number of submodules to skip first, for pagination.
    pub skip: Option<usize>,
}

/// How a submodule's checkout relates to what the superproject records, as `git submodule status` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubmoduleState {
    /// Not checked out: never initialized, or deinitialized (`git submodule status` prefix `-`).
    Uninitialized,
    /// Checked out at the commit the superproject's index records.
    UpToDate,
    /// Checked out at a different commit than the index records (prefix `+`).
    DifferentCommit,
    /// The submodule's path has unresolved merge conflicts in the index (prefix `U`).
    Conflicted,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SubmoduleEntry {
    /// Name from `.gitmodules`; often the same as `path`.
    pub name: String,
    /// Path of the submodule relative to the repository root.
    pub path: String,
    /// Configured URL (`.git/config` overrides `.gitmodules`), with any password redacted; `null` when
    /// it is missing or invalid. Relative URLs such as `../lib` are returned as written.
    pub url: Option<String>,
    /// Branch the submodule follows (`submodule.<name>.branch`), or `.` for "the superproject's current
    /// branch"; `null` when not set.
    pub branch: Option<String>,
    /// Full SHA of the commit the superproject's index records for the submodule; `null` when the path
    /// isn't in the index as a submodule or is conflicted.
    pub recorded_commit: Option<String>,
    /// Full SHA checked out in the submodule; `null` when it isn't checked out.
    pub checked_out_commit: Option<String>,
    /// How the checkout relates to `recorded_commit`.
    pub state: SubmoduleState,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListSubmodulesResult {
    /// Submodules declared in `.gitmodules`, ordered by path like `git submodule status`. Empty when
    /// there is no `.gitmodules`.
    pub submodules: Vec<SubmoduleEntry>,
    /// True if more submodules exist past this page.
    pub truncated: bool,
}

/// The submodules declared in `.gitmodules`, with recorded and checked-out commits, like `git submodule status`.
pub fn list_submodules(
    repo: &gix::Repository,
    args: ListSubmodulesArgs,
) -> anyhow::Result<ListSubmodulesResult> {
    let index = repo.index_or_empty()?;
    let mut submodules = match repo.submodules()? {
        Some(found) => found
            .map(|submodule| entry(&submodule, &index))
            .collect::<anyhow::Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    submodules.sort_by(|a, b| a.path.cmp(&b.path));
    let limit = page_limit(args.limit, DEFAULT_SUBMODULE_LIMIT, MAX_SUBMODULE_LIMIT);
    let (submodules, truncated) = paginate(
        submodules.into_iter().map(anyhow::Ok),
        args.skip.unwrap_or(0),
        limit,
    )?;
    Ok(ListSubmodulesResult {
        submodules,
        truncated,
    })
}

fn entry(
    submodule: &gix::Submodule<'_>,
    index: &gix::index::State,
) -> anyhow::Result<SubmoduleEntry> {
    let path = submodule.path()?;
    let (recorded, conflicted) = recorded_commit(index, path.as_bstr());
    let checked_out = checked_out_commit(submodule);
    let state = if conflicted {
        SubmoduleState::Conflicted
    } else if checked_out.is_none() {
        SubmoduleState::Uninitialized
    } else if checked_out == recorded {
        SubmoduleState::UpToDate
    } else {
        SubmoduleState::DifferentCommit
    };
    let url = submodule.url().ok().map(|url| {
        let url = url.to_bstring().to_string();
        redact_url_password(&url).unwrap_or(url)
    });
    Ok(SubmoduleEntry {
        name: submodule.name().to_string(),
        path: path.to_string(),
        url,
        branch: submodule.branch()?.map(|branch| match branch {
            // `branch = .` follows whatever branch the superproject has checked out.
            Branch::CurrentInSuperproject => ".".to_owned(),
            Branch::Name(name) => name.to_string(),
        }),
        recorded_commit: recorded.map(|id| id.to_string()),
        checked_out_commit: checked_out.map(|id| id.to_string()),
        state,
    })
}

/// The gitlink the index records at `path`, and whether the path is conflicted.
fn recorded_commit(index: &gix::index::State, path: &BStr) -> (Option<ObjectId>, bool) {
    let Some(range) = index.entry_range(path) else {
        return (None, false);
    };
    let entries = &index.entries()[range];
    let conflicted = entries.iter().any(|e| e.stage() != Stage::Unconflicted);
    let recorded = entries
        .iter()
        .find(|e| e.stage() == Stage::Unconflicted && e.mode == Mode::COMMIT)
        .map(|e| e.id);
    (recorded.filter(|_| !conflicted), conflicted)
}

/// HEAD of the submodule's checkout, or `None` when it isn't checked out (or can't be opened).
fn checked_out_commit(submodule: &gix::Submodule<'_>) -> Option<ObjectId> {
    let state = submodule.state().ok()?;
    if !state.worktree_checkout {
        return None;
    }
    match submodule.open() {
        Ok(Some(repo)) => repo.head_id().ok().map(|id| id.detach()),
        Ok(None) => None,
        Err(err) => {
            tracing::debug!(%err, name = %submodule.name(), "cannot open submodule");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        bare_repo, fixture_repo, git_stdout, open, run_git, try_git, write,
    };

    fn list(repo: &gix::Repository) -> ListSubmodulesResult {
        list_submodules(
            repo,
            ListSubmodulesArgs {
                limit: None,
                skip: None,
            },
        )
        .unwrap()
    }

    /// `(prefix, sha, path)` rows from `git submodule status`, the oracle for list_submodules.
    fn git_status(dir: &std::path::Path) -> Vec<(char, String, String)> {
        git_stdout(dir, &["submodule", "status"])
            .lines()
            .map(|line| {
                let prefix = line.chars().next().unwrap();
                let mut fields = line[1..].split(' ');
                (
                    prefix,
                    fields.next().unwrap().to_owned(),
                    fields.next().unwrap().to_owned(),
                )
            })
            .collect()
    }

    fn rows(res: &ListSubmodulesResult) -> Vec<(char, String, String)> {
        res.submodules
            .iter()
            .map(|s| {
                let (prefix, sha) = match s.state {
                    SubmoduleState::Uninitialized => ('-', s.recorded_commit.clone()),
                    SubmoduleState::UpToDate => (' ', s.recorded_commit.clone()),
                    SubmoduleState::DifferentCommit => ('+', s.checked_out_commit.clone()),
                    SubmoduleState::Conflicted => ('U', Some("0".repeat(40))),
                };
                (prefix, sha.unwrap(), s.path.clone())
            })
            .collect()
    }

    fn add_submodule(root: &std::path::Path, url: &str, path: &str) {
        run_git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                url,
                path,
            ],
        );
    }

    /// A superproject with submodules `vendor/lib` (declared first) and `other`, both of `lib`.
    fn superproject() -> (tempfile::TempDir, tempfile::TempDir, gix::Repository) {
        let (lib, _) = fixture_repo();
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let url = lib.path().to_str().unwrap();
        add_submodule(root, url, "vendor/lib");
        add_submodule(root, url, "other");
        run_git(
            root,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.other.branch",
                "main",
            ],
        );
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "add submodules"]);
        let repo = open(root);
        (dir, lib, repo)
    }

    #[test]
    fn states_match_git_submodule_status() {
        let (dir, _lib, repo) = superproject();
        let root = dir.path();
        let res = list(&repo);
        assert_eq!(rows(&res), git_status(root));
        assert!(
            res.submodules
                .iter()
                .all(|s| s.state == SubmoduleState::UpToDate)
        );
        assert_eq!(
            res.submodules[0].path, "other",
            "ordered by path, not by .gitmodules"
        );
        assert_eq!(res.submodules[0].branch.as_deref(), Some("main"));
        assert_eq!(res.submodules[1].branch, None);

        run_git(&root.join("other"), &["checkout", "-q", "HEAD~1"]);
        run_git(root, &["submodule", "deinit", "-q", "vendor/lib"]);
        let res = list(&open(root));
        assert_eq!(rows(&res), git_status(root));
        assert_eq!(res.submodules[0].state, SubmoduleState::DifferentCommit);
        assert_eq!(res.submodules[1].state, SubmoduleState::Uninitialized);
        assert!(res.submodules[1].checked_out_commit.is_none());
    }

    #[test]
    fn conflicted_submodule() {
        let (dir, _lib, _) = superproject();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "-b", "side"]);
        run_git(&root.join("other"), &["checkout", "-q", "HEAD~1"]);
        run_git(root, &["commit", "-q", "-am", "side moves other"]);
        run_git(root, &["checkout", "-q", "main"]);
        run_git(root, &["submodule", "update", "-q"]);
        write(&root.join("other"), "new.txt", "x");
        run_git(&root.join("other"), &["add", "."]);
        run_git(
            &root.join("other"),
            &["commit", "-q", "-m", "main moves other"],
        );
        run_git(root, &["commit", "-q", "-am", "main moves other"]);
        assert!(!try_git(root, &["merge", "-q", "side"]));
        let res = list(&open(root));
        let other = &res.submodules[0];
        assert_eq!(other.state, SubmoduleState::Conflicted);
        assert!(other.recorded_commit.is_none());
        assert_eq!(rows(&res)[0].0, git_status(root)[0].0);
    }

    #[test]
    fn urls_are_redacted_and_kept_relative() {
        let (dir, _lib, _) = superproject();
        let root = dir.path();
        run_git(
            root,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.vendor/lib.url",
                "../lib",
            ],
        );
        run_git(
            root,
            &[
                "config",
                "submodule.other.url",
                "https://bot:s3cret@example.com/lib.git",
            ],
        );
        let res = list(&open(root));
        assert_eq!(
            res.submodules[0].url.as_deref(),
            Some("https://bot:<redacted>@example.com/lib.git")
        );
        // `submodule add` registered an absolute URL in .git/config; without it, .gitmodules applies as written.
        assert_ne!(res.submodules[1].url.as_deref(), Some("../lib"));
        run_git(root, &["config", "--unset", "submodule.vendor/lib.url"]);
        let res = list(&open(root));
        assert_eq!(res.submodules[1].name, "vendor/lib");
        assert_eq!(res.submodules[1].url.as_deref(), Some("../lib"));
    }

    #[test]
    fn no_submodules_and_paging() {
        let (_dir, repo) = fixture_repo();
        let res = list(&repo);
        assert!(res.submodules.is_empty() && !res.truncated);
        let (_dir, bare) = bare_repo();
        assert!(list(&bare).submodules.is_empty());

        let (_dir, _lib, repo) = superproject();
        let page = list_submodules(
            &repo,
            ListSubmodulesArgs {
                limit: Some(1),
                skip: None,
            },
        )
        .unwrap();
        assert_eq!(page.submodules.len(), 1);
        assert!(page.truncated);
    }
}
