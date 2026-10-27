use anyhow::{Context, bail};
use gix::{
    ObjectId,
    bstr::{BString, ByteSlice},
    index::entry::{Mode, Stage},
    merge::{
        blob::builtin_driver::text::Labels,
        tree::{TreatAsUnresolved, apply_index_entries::RemovalMode},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ConflictKind, byte_budget, clip_utf8, in_progress, index_entry_kind, is_binary, octal_mode,
    page_limit, resolve_commit,
};

const DEFAULT_CONFLICT_LIMIT: usize = 100;
const MAX_CONFLICT_LIMIT: usize = 1000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConflictsArgs {
    /// Include the text of each version (base, ours, theirs) in its `content` (default false). Binary
    /// files and submodules never have content.
    #[serde(default)]
    pub include_content: bool,
    /// Byte budget shared by all `content` fields in the response (default 65536, max 1048576).
    pub max_bytes: Option<usize>,
    /// Maximum conflicted files to return (default 100, max 1000).
    pub limit: Option<usize>,
    /// Number of conflicted files to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConflictVersion {
    /// Full SHA of this version's blob (for a submodule, the commit it records).
    pub id: String,
    /// Git file mode in octal: `100644` file, `100755` executable, `120000` symlink, `160000` submodule.
    pub mode: String,
    /// This version's text when `include_content` is set; `null` without it, for binary files and
    /// submodules, and once the byte budget is used up. Invalid UTF-8 is replaced with U+FFFD.
    pub content: Option<String>,
    /// True when `content` was cut at the byte budget, or left out because the budget was used up.
    pub content_truncated: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConflictFile {
    /// Path relative to the repository root.
    pub path: String,
    /// Which sides changed, added or deleted the path, as `git status` reports it.
    pub conflict: ConflictKind,
    /// The common ancestor's version (index stage 1); `null` when the path didn't exist there.
    pub base: Option<ConflictVersion>,
    /// Our version (stage 2): the branch being merged into, or in a rebase the commits being rebased
    /// onto. `null` when our side deleted the path.
    pub ours: Option<ConflictVersion>,
    /// Their version (stage 3): the branch being merged, or the commit being replayed by a rebase,
    /// cherry-pick or revert. `null` when their side deleted the path.
    pub theirs: Option<ConflictVersion>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConflictsResult {
    /// The operation in progress, e.g. `Merge`, `Rebase`, `CherryPick` or `Revert`; `null` when none.
    /// Conflicts can exist without one, e.g. after `git stash pop`.
    pub in_progress: Option<String>,
    /// Conflicted files ordered by path. Empty when the index has no conflicts.
    pub files: Vec<ConflictFile>,
    /// Number of conflicted files before pagination.
    pub total: usize,
    /// True if more conflicted files exist past this page.
    pub truncated: bool,
}

/// The unresolved conflicts in the index, with the base, ours and theirs version of each file, like
/// `git ls-files -u` with `git status` conflict kinds.
pub fn conflicts(repo: &gix::Repository, args: ConflictsArgs) -> anyhow::Result<ConflictsResult> {
    let index = repo.index_or_empty()?;
    let page = Page::new(args.limit, args.skip, args.include_content, args.max_bytes);
    let (files, total, truncated) = page.render(repo, staged_files(&index))?;
    Ok(ConflictsResult {
        in_progress: in_progress(repo),
        files,
        total,
        truncated,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergePreviewArgs {
    /// The branch you would merge into (default `HEAD`). Any commit-ish works.
    pub ours: Option<String>,
    /// The branch or commit you would merge, e.g. `feature`.
    pub theirs: String,
    /// Merge histories that share no commit, like `git merge --allow-unrelated-histories` (default false).
    #[serde(default)]
    pub allow_unrelated: bool,
    /// Include the text of each version of the conflicted files in `content` (default false).
    #[serde(default)]
    pub include_content: bool,
    /// Byte budget shared by all `content` fields in the response (default 65536, max 1048576).
    pub max_bytes: Option<usize>,
    /// Maximum conflicted files to return (default 100, max 1000).
    pub limit: Option<usize>,
    /// Number of conflicted files to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MergePreviewResult {
    /// Full SHA of the commit `ours` resolved to.
    pub ours: String,
    /// Full SHA of the commit `theirs` resolved to.
    pub theirs: String,
    /// The merge bases used. Several for criss-cross histories, which are merged into a virtual base
    /// first as git does; empty for unrelated histories.
    pub merge_bases: Vec<String>,
    /// True when the merge completes without conflicts.
    pub clean: bool,
    /// True when `theirs` is already part of `ours`, so merging changes nothing ("Already up to date").
    pub up_to_date: bool,
    /// True when `ours` is part of `theirs`, so `git merge` would fast-forward rather than create a merge commit.
    pub fast_forward: bool,
    /// SHA of the merged tree, as `git merge-tree --write-tree` prints it; conflicted files in it contain
    /// conflict markers. It is computed in memory and not stored, so other tools can't read it.
    pub tree: String,
    /// Files that would be left conflicted, ordered by path, with the versions `conflicts` would show
    /// after running the merge.
    pub conflicts: Vec<ConflictFile>,
    /// Number of conflicted files before pagination.
    pub total: usize,
    /// True if more conflicted files exist past this page.
    pub truncated: bool,
}

/// Merge two commits in memory, like `git merge-tree --write-tree`, and report whether it's clean and
/// what would conflict. Nothing is written: new blobs and trees stay in an in-memory object store.
pub fn merge_preview(
    repo: &gix::Repository,
    args: MergePreviewArgs,
) -> anyhow::Result<MergePreviewResult> {
    let ours_name = args.ours.as_deref().unwrap_or("HEAD");
    let ours = resolve_commit(repo, Some(ours_name))?.id;
    let theirs = resolve_commit(repo, Some(&args.theirs))?.id;
    let merge_base = repo.merge_base(ours, theirs)?.map(|id| id.detach());
    if merge_base.is_none() && !args.allow_unrelated {
        bail!(
            "`{ours_name}` and `{}` share no history; set `allow_unrelated: true` to preview merging \
             them anyway, like `git merge --allow-unrelated-histories`",
            args.theirs
        );
    }

    // Merged blobs, the result tree and any virtual merge base are written as the merge runs. Keeping
    // them in memory leaves the object database untouched, so this stays a read-only operation.
    let memory = repo.clone().with_object_memory();
    let options = gix::merge::commit::Options::from(memory.tree_merge_options()?)
        .with_allow_missing_merge_base(args.allow_unrelated);
    let labels = Labels {
        ancestor: None,
        current: Some(ours_name.as_bytes().as_bstr()),
        other: Some(args.theirs.as_bytes().as_bstr()),
    };
    let mut outcome = memory
        .merge_commits(ours, theirs, labels, options)
        .context("the merge could not be computed")?;
    let tree = outcome.tree_merge.tree.write()?.detach();
    let mut index = memory.index_from_tree(&tree)?;
    outcome.tree_merge.index_changed_after_applying_conflicts(
        &mut index,
        TreatAsUnresolved::git(),
        RemovalMode::Prune,
    );

    let page = Page::new(args.limit, args.skip, args.include_content, args.max_bytes);
    let (conflicts, total, truncated) = page.render(&memory, staged_files(&index))?;
    Ok(MergePreviewResult {
        ours: ours.to_string(),
        theirs: theirs.to_string(),
        merge_bases: outcome
            .merge_bases
            .map(|bases| bases.into_iter().map(|id| id.to_string()).collect())
            .unwrap_or_default(),
        clean: !outcome
            .tree_merge
            .has_unresolved_conflicts(TreatAsUnresolved::git()),
        up_to_date: merge_base == Some(theirs),
        fast_forward: merge_base == Some(ours) && ours != theirs,
        tree: tree.to_string(),
        conflicts,
        total,
        truncated,
    })
}

/// A conflicted path with the index entries of its stages: base (1), ours (2) and theirs (3).
struct Staged {
    path: BString,
    stages: [Option<(ObjectId, Mode)>; 3],
}

/// Conflicted paths of `index` in path order.
fn staged_files(index: &gix::index::State) -> Vec<Staged> {
    let mut files: Vec<Staged> = Vec::new();
    for entry in index.entries() {
        let slot = match entry.stage() {
            Stage::Unconflicted => continue,
            Stage::Base => 0,
            Stage::Ours => 1,
            Stage::Theirs => 2,
        };
        let path = entry.path(index);
        if files.last().is_none_or(|file| file.path != path) {
            files.push(Staged {
                path: path.to_owned(),
                stages: [None; 3],
            });
        }
        if let Some(file) = files.last_mut() {
            file.stages[slot] = Some((entry.id, entry.mode));
        }
    }
    files
}

/// Pagination and content options shared by `conflicts` and `merge_preview`.
struct Page {
    limit: usize,
    skip: usize,
    /// Bytes of content still allowed; `None` when content wasn't requested.
    budget: Option<usize>,
}

impl Page {
    fn new(
        limit: Option<usize>,
        skip: Option<usize>,
        include_content: bool,
        max_bytes: Option<usize>,
    ) -> Self {
        Self {
            limit: page_limit(limit, DEFAULT_CONFLICT_LIMIT, MAX_CONFLICT_LIMIT),
            skip: skip.unwrap_or(0),
            budget: include_content.then(|| byte_budget(max_bytes)),
        }
    }

    /// One page of `files` as `(files, total, truncated)`, with content loaded for that page only.
    fn render(
        mut self,
        repo: &gix::Repository,
        files: Vec<Staged>,
    ) -> anyhow::Result<(Vec<ConflictFile>, usize, bool)> {
        let total = files.len();
        let page = files
            .into_iter()
            .skip(self.skip)
            .take(self.limit)
            .map(|file| self.conflict_file(repo, file))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let truncated = self.skip.saturating_add(page.len()) < total;
        Ok((page, total, truncated))
    }

    fn conflict_file(
        &mut self,
        repo: &gix::Repository,
        file: Staged,
    ) -> anyhow::Result<ConflictFile> {
        let [base, ours, theirs] = file.stages;
        let conflict = ConflictKind::from_stages(base.is_some(), ours.is_some(), theirs.is_some())
            .context("a conflicted path has no conflict stages")?;
        Ok(ConflictFile {
            path: file.path.to_string(),
            conflict,
            base: base.map(|v| self.version(repo, v)).transpose()?,
            ours: ours.map(|v| self.version(repo, v)).transpose()?,
            theirs: theirs.map(|v| self.version(repo, v)).transpose()?,
        })
    }

    fn version(
        &mut self,
        repo: &gix::Repository,
        (id, mode): (ObjectId, Mode),
    ) -> anyhow::Result<ConflictVersion> {
        let kind = index_entry_kind(mode);
        let (content, content_truncated) = match self.budget {
            Some(left) if kind != gix::object::tree::EntryKind::Commit => {
                let blob = repo.find_blob(id)?;
                if is_binary(&blob.data) {
                    (None, false)
                } else {
                    let text = String::from_utf8_lossy(&blob.data);
                    let (clipped, cut) = clip_utf8(&text, left);
                    self.budget = Some(left - clipped.len());
                    let omitted = cut && clipped.is_empty();
                    ((!omitted).then(|| clipped.to_owned()), cut)
                }
            }
            _ => (None, false),
        };
        Ok(ConflictVersion {
            id: id.to_string(),
            mode: octal_mode(kind),
            content,
            content_truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use super::*;
    use crate::tools::testutil::{
        bare_repo, fixture_repo, git_output, git_stdout, open, run_git, run_git_with_env, try_git,
        write,
    };

    fn preview(
        repo: &gix::Repository,
        ours: &str,
        theirs: &str,
    ) -> anyhow::Result<MergePreviewResult> {
        merge_preview(
            repo,
            MergePreviewArgs {
                ours: Some(ours.into()),
                theirs: theirs.into(),
                allow_unrelated: false,
                include_content: false,
                max_bytes: None,
                limit: None,
                skip: None,
            },
        )
    }

    fn conflict_args() -> ConflictsArgs {
        ConflictsArgs {
            include_content: false,
            max_bytes: None,
            limit: None,
            skip: None,
        }
    }

    /// From a shared base, `main` edits a.txt, deletes c.txt and adds e.txt; `side` edits a.txt
    /// differently, modifies c.txt, adds d.txt and an image that `main` also adds differently.
    fn diverged() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(root, "a.txt", "one\ntwo\nthree\n");
        write(root, "c.txt", "gone\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "base"]);
        run_git(root, &["checkout", "-q", "-b", "side"]);
        write(root, "a.txt", "one\nTWO side\nthree\n");
        write(root, "c.txt", "side edit\n");
        write(root, "d.txt", "new\n");
        write(root, "img.bin", b"\x00side");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "side"]);
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "a.txt", "one\nTWO main\nthree\n");
        write(root, "e.txt", "other\n");
        write(root, "img.bin", b"\x00main");
        run_git(root, &["rm", "-q", "c.txt"]);
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "main"]);
        let repo = open(root);
        (dir, repo)
    }

    /// The tree and `mode id stage path` rows `git merge-tree --write-tree -z` reports.
    fn git_merge_tree(dir: &Path, args: &[&str]) -> (String, Vec<String>) {
        let mut cmd = vec!["merge-tree", "--write-tree", "-z"];
        cmd.extend_from_slice(args);
        let out = git_output(dir, &cmd, b"");
        assert!(
            out.status.code().unwrap() <= 1,
            "git merge-tree {args:?} failed"
        );
        let text = String::from_utf8(out.stdout).unwrap();
        let mut fields = text.split('\0');
        let tree = fields.next().unwrap().to_owned();
        let stages = fields
            .take_while(|f| !f.is_empty())
            .map(|f| f.replace('\t', " "))
            .collect();
        (tree, stages)
    }

    /// Our conflicts as `mode id stage path` rows, in index order.
    fn stage_rows(files: &[ConflictFile]) -> Vec<String> {
        files
            .iter()
            .flat_map(|f| {
                [&f.base, &f.ours, &f.theirs]
                    .into_iter()
                    .enumerate()
                    .filter_map(move |(n, v)| {
                        v.as_ref()
                            .map(|v| format!("{} {} {} {}", v.mode, v.id, n + 1, f.path))
                    })
            })
            .collect()
    }

    /// Every file under `.git/objects` with its size, to prove nothing was written.
    fn object_store(root: &Path) -> BTreeMap<String, u64> {
        fn walk(dir: &Path, out: &mut BTreeMap<String, u64>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(path.display().to_string(), path.metadata().unwrap().len());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&root.join(".git/objects"), &mut out);
        out
    }

    #[test]
    fn conflicted_merge_matches_git_merge_tree() {
        let (dir, repo) = diverged();
        let root = dir.path();
        let before = object_store(root);
        let res = preview(&repo, "main", "side").unwrap();
        assert_eq!(
            object_store(root),
            before,
            "a preview must not write objects"
        );

        let (tree, stages) = git_merge_tree(root, &["main", "side"]);
        assert_eq!(res.tree, tree);
        assert_eq!(stage_rows(&res.conflicts), stages);
        assert!(!res.clean && !res.up_to_date && !res.fast_forward);
        let kinds: Vec<_> = res
            .conflicts
            .iter()
            .map(|f| (f.path.as_str(), f.conflict))
            .collect();
        assert_eq!(
            kinds,
            [
                ("a.txt", ConflictKind::BothModified),
                ("c.txt", ConflictKind::DeletedByUs),
                ("img.bin", ConflictKind::BothAdded),
            ]
        );
        assert_eq!(
            res.merge_bases,
            [git_stdout(root, &["merge-base", "main", "side"]).trim()]
        );
        assert_eq!((res.total, res.truncated), (3, false));
    }

    #[test]
    fn clean_up_to_date_and_fast_forward() {
        let (dir, repo) = diverged();
        let root = dir.path();
        let clean = preview(&repo, "main~1", "side").unwrap();
        assert!(clean.clean && clean.conflicts.is_empty());
        assert_eq!(clean.tree, git_merge_tree(root, &["main~1", "side"]).0);

        let up_to_date = preview(&repo, "main", "main~1").unwrap();
        assert!(up_to_date.clean && up_to_date.up_to_date && !up_to_date.fast_forward);
        let forward = preview(&repo, "main~1", "main").unwrap();
        assert!(forward.clean && forward.fast_forward && !forward.up_to_date);
        assert_eq!(
            forward.tree,
            git_stdout(root, &["rev-parse", "main^{tree}"]).trim()
        );
    }

    #[test]
    fn criss_cross_histories_use_every_merge_base() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let at = |n: u32| format!("2026-01-0{n}T00:00:00Z");
        let commit = |file: &str, text: &str, n: u32| {
            write(root, file, text);
            run_git(root, &["add", "."]);
            run_git_with_env(
                root,
                &["commit", "-q", "-m", file],
                &[("GIT_COMMITTER_DATE", &at(n))],
            );
        };
        run_git(root, &["checkout", "-q", "-b", "left"]);
        commit("l.txt", "l\n", 1);
        run_git(root, &["checkout", "-q", "-b", "right", "main"]);
        commit("r.txt", "r\n", 2);
        run_git(root, &["checkout", "-q", "-b", "left2", "left"]);
        run_git(root, &["merge", "-q", "--no-edit", "right"]);
        run_git(root, &["checkout", "-q", "right"]);
        run_git(root, &["merge", "-q", "--no-edit", "left"]);
        commit("a.txt", "right side\n", 3);
        run_git(root, &["checkout", "-q", "left2"]);
        commit("a.txt", "left side\n", 4);
        let repo = open(root);
        let res = preview(&repo, "left2", "right").unwrap();
        assert_eq!(res.merge_bases.len(), 2);
        let (tree, stages) = git_merge_tree(root, &["left2", "right"]);
        assert_eq!(stage_rows(&res.conflicts), stages);
        assert_eq!(res.tree, tree);
    }

    #[test]
    fn unrelated_histories_need_opting_in() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "--orphan", "island"]);
        run_git(root, &["rm", "-q", "-r", "--cached", "."]);
        write(root, "island.txt", "alone\n");
        run_git(root, &["add", "island.txt"]);
        run_git(root, &["commit", "-q", "-m", "unrelated root"]);
        let repo = open(root);
        let err = format!("{:#}", preview(&repo, "main", "island").unwrap_err());
        assert!(err.contains("allow_unrelated"), "{err}");

        let res = merge_preview(
            &repo,
            MergePreviewArgs {
                allow_unrelated: true,
                ..preview_args("main", "island")
            },
        )
        .unwrap();
        assert!(res.merge_bases.is_empty());
        let (tree, _) = git_merge_tree(root, &["--allow-unrelated-histories", "main", "island"]);
        assert_eq!(res.tree, tree);
    }

    fn preview_args(ours: &str, theirs: &str) -> MergePreviewArgs {
        MergePreviewArgs {
            ours: Some(ours.into()),
            theirs: theirs.into(),
            allow_unrelated: false,
            include_content: false,
            max_bytes: None,
            limit: None,
            skip: None,
        }
    }

    #[test]
    fn preview_content_paging_and_errors() {
        let (_dir, repo) = diverged();
        let res = merge_preview(
            &repo,
            MergePreviewArgs {
                include_content: true,
                limit: Some(1),
                ..preview_args("main", "side")
            },
        )
        .unwrap();
        assert_eq!((res.conflicts.len(), res.total), (1, 3));
        assert!(res.truncated);
        let a = &res.conflicts[0];
        assert_eq!(
            a.ours.as_ref().unwrap().content.as_deref(),
            Some("one\nTWO main\nthree\n")
        );
        assert_eq!(
            a.theirs.as_ref().unwrap().content.as_deref(),
            Some("one\nTWO side\nthree\n")
        );

        let err = format!("{:#}", preview(&repo, "main", "nope").unwrap_err());
        assert!(err.contains("cannot resolve `nope`"), "{err}");
        let parsed: MergePreviewArgs = serde_json::from_str(r#"{"theirs":"x"}"#).unwrap();
        assert!(parsed.ours.is_none() && !parsed.allow_unrelated && !parsed.include_content);
    }

    #[test]
    fn conflicts_match_git_after_a_real_merge() {
        let (dir, _) = diverged();
        let root = dir.path();
        assert!(!try_git(root, &["merge", "-q", "side"]));
        let repo = open(root);
        let res = conflicts(&repo, conflict_args()).unwrap();
        assert_eq!(res.in_progress.as_deref(), Some("Merge"));

        let git_rows: Vec<String> = git_stdout(root, &["ls-files", "-u", "-z"])
            .split('\0')
            .filter(|row| !row.is_empty())
            .map(|row| row.replace('\t', " "))
            .collect();
        assert_eq!(stage_rows(&res.files), git_rows);
        // git status --porcelain=v2 marks conflicts with `u <XY> ...`.
        let codes: Vec<String> = git_stdout(root, &["status", "--porcelain=v2"])
            .lines()
            .filter_map(|l| l.strip_prefix("u ").map(|rest| rest[..2].to_owned()))
            .collect();
        let ours: Vec<&str> = res
            .files
            .iter()
            .map(|f| match f.conflict {
                ConflictKind::BothModified => "UU",
                ConflictKind::BothAdded => "AA",
                ConflictKind::DeletedByUs => "DU",
                ConflictKind::DeletedByThem => "UD",
                ConflictKind::AddedByUs => "AU",
                ConflictKind::AddedByThem => "UA",
                ConflictKind::BothDeleted => "DD",
            })
            .collect();
        assert_eq!(ours, codes);
        assert!(!res.truncated && res.total == 3);
    }

    #[test]
    fn conflict_content_respects_the_budget() {
        let (dir, _) = diverged();
        let root = dir.path();
        assert!(!try_git(root, &["merge", "-q", "side"]));
        let repo = open(root);
        let full = conflicts(
            &repo,
            ConflictsArgs {
                include_content: true,
                ..conflict_args()
            },
        )
        .unwrap();
        let a = &full.files[0];
        assert_eq!(
            a.ours.as_ref().unwrap().content.as_deref(),
            Some(git_stdout(root, &["show", ":2:a.txt"]).as_str())
        );
        let img = full.files.iter().find(|f| f.path == "img.bin").unwrap();
        assert!(
            img.ours.as_ref().unwrap().content.is_none(),
            "binary content is never inlined"
        );

        let tight = conflicts(
            &repo,
            ConflictsArgs {
                include_content: true,
                max_bytes: Some(20),
                ..conflict_args()
            },
        )
        .unwrap();
        let a = &tight.files[0];
        assert_eq!(
            a.base.as_ref().unwrap().content.as_deref(),
            Some("one\ntwo\nthree\n")
        );
        assert!(a.ours.as_ref().unwrap().content_truncated);
        assert!(
            a.theirs.as_ref().unwrap().content.is_none()
                && a.theirs.as_ref().unwrap().content_truncated
        );
    }

    #[test]
    fn no_conflicts_bare_and_paging() {
        let (_dir, repo) = fixture_repo();
        let res = conflicts(&repo, conflict_args()).unwrap();
        assert!(res.files.is_empty() && res.in_progress.is_none() && res.total == 0);
        let (_dir, bare) = bare_repo();
        assert!(conflicts(&bare, conflict_args()).unwrap().files.is_empty());

        let (dir, _) = diverged();
        assert!(!try_git(dir.path(), &["merge", "-q", "side"]));
        let repo = open(dir.path());
        let page = conflicts(
            &repo,
            ConflictsArgs {
                limit: Some(1),
                skip: Some(2),
                ..conflict_args()
            },
        )
        .unwrap();
        assert_eq!(page.files[0].path, "img.bin");
        assert!(!page.truncated);
    }
}
