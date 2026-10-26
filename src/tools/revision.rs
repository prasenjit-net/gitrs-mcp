use anyhow::Context;
use gix::{
    ObjectId,
    revision::{plumbing::Spec, walk::Sorting},
    traverse::commit::simple::CommitTimeOrder,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{LogEntry, page_limit, resolve_commit};

const DEFAULT_COMPARE_LIMIT: usize = 20;
const MAX_COMPARE_LIMIT: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevParseArgs {
    /// A revision as `git rev-parse` accepts it: a full or abbreviated SHA, a ref name, `HEAD~2`,
    /// `v1.0^{tree}`, `main:src/lib.rs`, `:/fix typo`, or a range such as `main..feature`,
    /// `main...feature`, `^main`, `HEAD^@` or `HEAD^!`.
    pub spec: String,
}

/// The shape of a revision spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpecKind {
    /// One object, e.g. `HEAD~2` or `main:src/lib.rs`. `ids` has one entry.
    Single,
    /// `^rev`: everything reachable from `rev` is excluded. `ids` has one entry.
    Exclude,
    /// `a..b`: commits reachable from `b` but not from `a`. `ids` is `[a, b]`.
    Range,
    /// `a...b`: commits reachable from either side but not both. `ids` is `[a, b]` and `merge_base` is set.
    MergeBaseRange,
    /// `rev^@`: all parents of `rev`, but not `rev` itself. `ids` has one entry.
    IncludeOnlyParents,
    /// `rev^!`: `rev` without its parents. `ids` has one entry.
    ExcludeParents,
}

/// A git object type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObjectType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl From<gix::object::Kind> for ObjectType {
    fn from(kind: gix::object::Kind) -> Self {
        match kind {
            gix::object::Kind::Commit => Self::Commit,
            gix::object::Kind::Tree => Self::Tree,
            gix::object::Kind::Blob => Self::Blob,
            gix::object::Kind::Tag => Self::Tag,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RevParseResult {
    /// The shape of the spec: one object or a range.
    pub kind: SpecKind,
    /// Full SHAs the spec resolved to, in the order they appear in the spec.
    pub ids: Vec<String>,
    /// Type of the first object in `ids`; range endpoints are commits.
    pub object_type: ObjectType,
    /// For `merge_base_range`: the best common ancestor of both sides. `null` for other kinds,
    /// or when the two histories share no commit.
    pub merge_base: Option<String>,
}

pub fn rev_parse(repo: &gix::Repository, args: RevParseArgs) -> anyhow::Result<RevParseResult> {
    let spec = repo
        .rev_parse(args.spec.as_str())
        .with_context(|| {
            format!(
                "cannot resolve `{}`; pass a SHA, ref name, an expression such as HEAD~2, or a range such as main..feature",
                args.spec
            )
        })?
        .detach();
    let (kind, ids, merge_base) = match spec {
        Spec::Include(id) => (SpecKind::Single, vec![id], None),
        Spec::Exclude(id) => (SpecKind::Exclude, vec![id], None),
        Spec::Range { from, to } => (SpecKind::Range, vec![from, to], None),
        Spec::Merge { theirs, ours } => {
            let base = repo.merge_base(theirs, ours)?.map(|id| id.detach());
            (SpecKind::MergeBaseRange, vec![theirs, ours], base)
        }
        Spec::IncludeOnlyParents(id) => (SpecKind::IncludeOnlyParents, vec![id], None),
        Spec::ExcludeParents(id) => (SpecKind::ExcludeParents, vec![id], None),
    };
    Ok(RevParseResult {
        kind,
        object_type: repo.find_header(ids[0])?.kind().into(),
        ids: ids.iter().map(ObjectId::to_string).collect(),
        merge_base: merge_base.map(|id| id.to_string()),
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompareRefsArgs {
    /// The reference point, usually the branch you'd merge into (e.g. `main`). Any revision works.
    pub base: String,
    /// The revision to compare against `base` (e.g. `feature` or `HEAD`).
    pub head: String,
    /// Maximum commits listed in each of `ahead_commits` and `behind_commits` (default 20, max 500).
    /// `ahead` and `behind` always count everything.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CompareRefsResult {
    /// Full SHA `base` resolved to.
    pub base: String,
    /// Full SHA `head` resolved to.
    pub head: String,
    /// Best common ancestor of `base` and `head`; `null` when the histories share no commit.
    pub merge_base: Option<String>,
    /// Number of commits in `head` that are not in `base` (`git rev-list --count base..head`).
    pub ahead: usize,
    /// Number of commits in `base` that are not in `head` (`git rev-list --count head..base`).
    pub behind: usize,
    /// The newest `ahead` commits, newest first.
    pub ahead_commits: Vec<LogEntry>,
    /// The newest `behind` commits, newest first.
    pub behind_commits: Vec<LogEntry>,
    /// True if either commit list was cut off at `limit`.
    pub truncated: bool,
}

/// How far two revisions have diverged.
pub fn compare_refs(
    repo: &gix::Repository,
    args: CompareRefsArgs,
) -> anyhow::Result<CompareRefsResult> {
    let base = resolve_commit(repo, Some(&args.base))?.id;
    let head = resolve_commit(repo, Some(&args.head))?.id;
    let limit = page_limit(args.limit, DEFAULT_COMPARE_LIMIT, MAX_COMPARE_LIMIT);

    let (ahead, ahead_commits) = only_in(repo, head, base, limit)?;
    let (behind, behind_commits) = only_in(repo, base, head, limit)?;
    Ok(CompareRefsResult {
        base: base.to_string(),
        head: head.to_string(),
        merge_base: repo.merge_base(base, head)?.map(|id| id.to_string()),
        truncated: ahead > ahead_commits.len() || behind > behind_commits.len(),
        ahead,
        behind,
        ahead_commits,
        behind_commits,
    })
}

/// Count the commits reachable from `tip` but not from `hidden`, keeping the newest `limit` of them.
fn only_in(
    repo: &gix::Repository,
    tip: ObjectId,
    hidden: ObjectId,
    limit: usize,
) -> anyhow::Result<(usize, Vec<LogEntry>)> {
    let walk = repo
        .rev_walk([tip])
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .with_hidden([hidden])
        .all()?;
    let mut count = 0;
    let mut commits = Vec::new();
    for info in walk {
        let info = info?;
        if commits.len() < limit {
            commits.push(LogEntry::from_commit(&info.object()?)?);
        }
        count += 1;
    }
    Ok((count, commits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{fixture_repo, git_stdout, open, run_git, write};

    fn parse(repo: &gix::Repository, spec: &str) -> anyhow::Result<RevParseResult> {
        rev_parse(repo, RevParseArgs { spec: spec.into() })
    }

    fn git_rev(dir: &std::path::Path, spec: &str) -> String {
        git_stdout(dir, &["rev-parse", spec]).trim().to_string()
    }

    /// `main` and `feature` both move on after branching: two commits ahead, one behind.
    fn diverged() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["checkout", "-q", "feature"]);
        for n in ["f1", "f2"] {
            write(root, &format!("{n}.txt"), n);
            run_git(root, &["add", "."]);
            run_git(root, &["commit", "-q", "-m", n]);
        }
        run_git(root, &["checkout", "-q", "main"]);
        write(root, "m1.txt", "m1");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "m1"]);
        let repo = open(root);
        (dir, repo)
    }

    #[test]
    fn single_specs_match_git() {
        let (dir, repo) = fixture_repo();
        for spec in [
            "HEAD",
            "HEAD~1",
            "v1",
            "main^{tree}",
            "main:a.txt",
            "feature",
        ] {
            let res = parse(&repo, spec).unwrap();
            assert_eq!(res.kind, SpecKind::Single, "{spec}");
            assert_eq!(res.ids, [git_rev(dir.path(), spec)], "{spec}");
        }
        assert_eq!(
            parse(&repo, "HEAD").unwrap().object_type,
            ObjectType::Commit
        );
        assert_eq!(
            parse(&repo, "HEAD^{tree}").unwrap().object_type,
            ObjectType::Tree
        );
        assert_eq!(
            parse(&repo, "HEAD:a.txt").unwrap().object_type,
            ObjectType::Blob
        );
        let short = &git_rev(dir.path(), "HEAD")[..8];
        assert_eq!(
            parse(&repo, short).unwrap().ids,
            [git_rev(dir.path(), "HEAD")]
        );
    }

    #[test]
    fn annotated_tag_is_a_tag_object() {
        let (dir, _) = fixture_repo();
        run_git(dir.path(), &["tag", "-a", "v2", "-m", "release"]);
        let res = parse(&open(dir.path()), "v2").unwrap();
        assert_eq!(res.object_type, ObjectType::Tag);
        assert_eq!(res.ids, [git_rev(dir.path(), "v2")]);
    }

    #[test]
    fn range_specs() {
        let (dir, repo) = diverged();
        let root = dir.path();
        let main = git_rev(root, "main");
        let feature = git_rev(root, "feature");

        let res = parse(&repo, "main..feature").unwrap();
        assert_eq!(res.kind, SpecKind::Range);
        assert_eq!(res.ids, [main.clone(), feature.clone()]);
        assert!(res.merge_base.is_none());

        let res = parse(&repo, "main...feature").unwrap();
        assert_eq!(res.kind, SpecKind::MergeBaseRange);
        assert_eq!(res.ids, [main.clone(), feature]);
        assert_eq!(
            res.merge_base,
            Some(
                git_stdout(root, &["merge-base", "main", "feature"])
                    .trim()
                    .to_string()
            )
        );

        assert_eq!(parse(&repo, "^main").unwrap().kind, SpecKind::Exclude);
        assert_eq!(
            parse(&repo, "main^@").unwrap().kind,
            SpecKind::IncludeOnlyParents
        );
        let res = parse(&repo, "main^!").unwrap();
        assert_eq!(res.kind, SpecKind::ExcludeParents);
        assert_eq!(res.ids, [main]);
    }

    #[test]
    fn unknown_spec_is_actionable() {
        let (_dir, repo) = fixture_repo();
        let err = format!("{:#}", parse(&repo, "no-such-branch").unwrap_err());
        assert!(err.contains("cannot resolve `no-such-branch`"), "{err}");
    }

    #[test]
    fn compare_matches_rev_list() {
        let (dir, repo) = diverged();
        let root = dir.path();
        let res = compare_refs(
            &repo,
            CompareRefsArgs {
                base: "main".into(),
                head: "feature".into(),
                limit: None,
            },
        )
        .unwrap();
        let counts = git_stdout(
            root,
            &["rev-list", "--left-right", "--count", "main...feature"],
        );
        assert_eq!(counts.trim(), format!("{}\t{}", res.behind, res.ahead));
        assert_eq!((res.ahead, res.behind), (2, 1));
        let ahead: Vec<_> = res.ahead_commits.iter().map(|c| c.id.clone()).collect();
        let git_ahead: Vec<_> = git_stdout(root, &["rev-list", "main..feature"])
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(ahead, git_ahead);
        assert_eq!(res.behind_commits[0].summary, "m1");
        assert_eq!(
            res.merge_base,
            Some(
                git_stdout(root, &["merge-base", "main", "feature"])
                    .trim()
                    .to_string()
            )
        );
        assert!(!res.truncated);
    }

    #[test]
    fn compare_caps_lists_but_not_counts() {
        let (_dir, repo) = diverged();
        let res = compare_refs(
            &repo,
            CompareRefsArgs {
                base: "main".into(),
                head: "feature".into(),
                limit: Some(1),
            },
        )
        .unwrap();
        assert_eq!(res.ahead, 2);
        assert_eq!(res.ahead_commits.len(), 1);
        assert_eq!(res.ahead_commits[0].summary, "f2");
        assert!(res.truncated);
    }

    #[test]
    fn compare_identical_and_unrelated() {
        let (dir, repo) = fixture_repo();
        let same = compare_refs(
            &repo,
            CompareRefsArgs {
                base: "main".into(),
                head: "feature".into(),
                limit: None,
            },
        )
        .unwrap();
        assert_eq!((same.ahead, same.behind), (0, 0));
        assert_eq!(same.merge_base.as_deref(), Some(same.base.as_str()));

        let root = dir.path();
        run_git(root, &["checkout", "-q", "--orphan", "island"]);
        run_git(root, &["commit", "-q", "-m", "unrelated root"]);
        let res = compare_refs(
            &open(root),
            CompareRefsArgs {
                base: "main".into(),
                head: "island".into(),
                limit: None,
            },
        )
        .unwrap();
        assert!(res.merge_base.is_none());
        assert_eq!((res.ahead, res.behind), (1, 2));

        let err = compare_refs(
            &repo,
            CompareRefsArgs {
                base: "main".into(),
                head: "nope".into(),
                limit: None,
            },
        );
        assert!(err.is_err());
    }
}
