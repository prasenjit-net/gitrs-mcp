use anyhow::Context;
use gix::commit::describe::SelectRef;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ObjectType, Signature, resolve_commit};

#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    #[default]
    Branches,
    Tags,
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRefsArgs {
    /// Which refs to list (default `branches`).
    #[serde(default)]
    pub kind: RefKind,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RefEntry {
    /// Full name, e.g. `refs/heads/main`.
    pub name: String,
    /// Short name, e.g. `main`.
    pub short_name: String,
    /// Commit the ref ultimately points to (annotated tags are peeled).
    pub target: Option<String>,
    /// True for the branch HEAD is on.
    pub is_head: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListRefsResult {
    /// Matching refs, sorted by full name.
    pub refs: Vec<RefEntry>,
}

pub fn list_refs(repo: &gix::Repository, args: ListRefsArgs) -> anyhow::Result<ListRefsResult> {
    let head = repo.head_name()?;
    let platform = repo.references()?;
    let iter = match args.kind {
        RefKind::Branches => platform.local_branches()?,
        RefKind::Tags => platform.tags()?,
        RefKind::All => platform.all()?,
    };

    let mut refs = Vec::new();
    for r in iter {
        let mut r = r.map_err(|e| anyhow::anyhow!(e))?;
        let name = r.name().to_owned();
        refs.push(RefEntry {
            short_name: name.shorten().to_string(),
            is_head: head.as_ref() == Some(&name),
            target: r.peel_to_id().ok().map(|id| id.to_string()),
            name: name.as_bstr().to_string(),
        });
    }
    Ok(ListRefsResult { refs })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShowTagArgs {
    /// Tag name, e.g. `v1.0` or `refs/tags/v1.0`.
    pub name: String,
}

/// How a tag was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TagKind {
    /// A tag object with a tagger, date and message (`git tag -a` or `-s`).
    Annotated,
    /// A plain ref pointing straight at an object (`git tag <name>`).
    Lightweight,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ShowTagResult {
    /// Full ref name, e.g. `refs/tags/v1.0`.
    pub name: String,
    /// Whether the tag is annotated (a tag object) or lightweight (a plain ref).
    pub kind: TagKind,
    /// Full SHA of the tag object; `null` for a lightweight tag.
    pub tag_id: Option<String>,
    /// Full SHA of the object the tag points at: the tag object's target, or the ref's target for a
    /// lightweight tag.
    pub target: String,
    /// Type of `target`; `tag` when an annotated tag points at another tag.
    pub target_type: ObjectType,
    /// Full SHA after peeling every tag layer, usually a commit (`git rev-parse <tag>^{}`).
    pub peeled: String,
    /// Type of `peeled`.
    pub peeled_type: ObjectType,
    /// Who created the annotated tag, and when; `null` for a lightweight tag or a tag object without a tagger.
    pub tagger: Option<Signature>,
    /// The annotated tag's message, without any signature block; `null` for a lightweight tag.
    pub message: Option<String>,
    /// True when the annotated tag carries a PGP or SSH signature. The signature is not verified.
    pub signed: bool,
}

/// Details of one tag: annotated or lightweight, what it points at, and the tagger and message.
pub fn show_tag(repo: &gix::Repository, args: ShowTagArgs) -> anyhow::Result<ShowTagResult> {
    let full_name = if args.name.starts_with("refs/") {
        args.name.clone()
    } else {
        format!("refs/tags/{}", args.name)
    };
    let not_found = || {
        format!(
            "no tag named `{}`; list_refs with kind `tags` shows the tags that exist",
            args.name
        )
    };
    let mut reference = repo
        .try_find_reference(full_name.as_str())
        .with_context(not_found)?
        .with_context(not_found)?;
    let direct = reference.follow_to_object()?.object()?;
    let peeled = direct.clone().peel_tags_to_end()?;
    let name = reference.name().as_bstr().to_string();

    if direct.kind != gix::object::Kind::Tag {
        return Ok(ShowTagResult {
            name,
            kind: TagKind::Lightweight,
            tag_id: None,
            target: direct.id.to_string(),
            target_type: direct.kind.into(),
            peeled: peeled.id.to_string(),
            peeled_type: peeled.kind.into(),
            tagger: None,
            message: None,
            signed: false,
        });
    }
    let tag = direct.into_tag();
    let decoded = tag.decode()?;
    Ok(ShowTagResult {
        name,
        kind: TagKind::Annotated,
        tag_id: Some(tag.id.to_string()),
        target: decoded.target().to_string(),
        target_type: decoded.target_kind.into(),
        peeled: peeled.id.to_string(),
        peeled_type: peeled.kind.into(),
        tagger: decoded.tagger()?.map(Signature::from_ref).transpose()?,
        message: Some(decoded.message.to_string()),
        signed: decoded.signature.is_some(),
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeArgs {
    /// Commit to describe (default `HEAD`).
    pub rev: Option<String>,
    /// Also consider lightweight tags, like `git describe --tags` (default false: annotated tags only).
    #[serde(default)]
    pub tags: bool,
    /// Always use the long form `<tag>-<n>-g<sha>`, even when the commit is tagged, like `git describe --long`.
    #[serde(default)]
    pub long: bool,
    /// Follow only the first parent of merges, like `git describe --first-parent` (default false).
    #[serde(default)]
    pub first_parent: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DescribeResult {
    /// What `git describe --always` prints: `<tag>` when the commit is tagged, `<tag>-<n>-g<short sha>`
    /// when it is `n` commits past the tag, or just the short SHA when no tag is reachable.
    pub description: String,
    /// Short name of the nearest reachable tag; `null` when no tag is reachable.
    pub tag: Option<String>,
    /// Number of commits in the described commit's history that aren't in `tag`'s; 0 without a tag.
    pub depth: u32,
    /// True when the commit itself carries `tag`.
    pub exact: bool,
    /// Full SHA of the described commit.
    pub id: String,
}

/// Name a commit after the nearest tag in its history, like `git describe --always`.
pub fn describe(repo: &gix::Repository, args: DescribeArgs) -> anyhow::Result<DescribeResult> {
    let commit = resolve_commit(repo, args.rev.as_deref())?;
    let names = if args.tags {
        SelectRef::AllTags
    } else {
        SelectRef::AnnotatedTags
    };
    let resolution = commit
        .describe()
        .names(names)
        .traverse_first_parent(args.first_parent)
        .id_as_fallback(true)
        .try_resolve()?
        .context("describe produced no result even with the SHA fallback")?;
    let mut format = resolution.format()?;
    format.long(args.long);
    let tag = format.name.as_ref().map(ToString::to_string);
    Ok(DescribeResult {
        description: format.to_string(),
        exact: tag.is_some() && format.is_exact_match(),
        depth: if tag.is_some() { format.depth } else { 0 },
        tag,
        id: commit.id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{
        empty_repo, fixture_repo, git_stdout, open, run_git, run_git_with_env, write,
    };

    #[test]
    fn lists_branches_with_head_marker() {
        let (_dir, repo) = fixture_repo();
        let res = list_refs(
            &repo,
            ListRefsArgs {
                kind: RefKind::Branches,
            },
        )
        .unwrap();
        let names: Vec<_> = res
            .refs
            .iter()
            .map(|r| (r.short_name.as_str(), r.is_head))
            .collect();
        assert_eq!(names, [("feature", false), ("main", true)]);
    }

    #[test]
    fn lists_tags() {
        let (_dir, repo) = fixture_repo();
        let res = list_refs(
            &repo,
            ListRefsArgs {
                kind: RefKind::Tags,
            },
        )
        .unwrap();
        assert_eq!(res.refs.len(), 1);
        assert_eq!(res.refs[0].name, "refs/tags/v1");
    }

    fn tag(repo: &gix::Repository, name: &str) -> anyhow::Result<ShowTagResult> {
        show_tag(repo, ShowTagArgs { name: name.into() })
    }

    fn rev(dir: &std::path::Path, spec: &str) -> String {
        git_stdout(dir, &["rev-parse", spec]).trim().to_owned()
    }

    #[test]
    fn lightweight_tag() {
        let (dir, repo) = fixture_repo();
        let res = tag(&repo, "v1").unwrap();
        assert_eq!(res.name, "refs/tags/v1");
        assert_eq!(res.kind, TagKind::Lightweight);
        assert_eq!(res.tag_id, None);
        assert_eq!(res.target, rev(dir.path(), "v1"));
        assert_eq!(res.target_type, ObjectType::Commit);
        assert_eq!(res.peeled, res.target);
        assert!(res.tagger.is_none() && res.message.is_none() && !res.signed);
        assert_eq!(tag(&repo, "refs/tags/v1").unwrap().target, res.target);
    }

    #[test]
    fn annotated_and_nested_tags_match_git() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(
            root,
            &["tag", "-a", "v2", "-m", "Release 2\n\nNotes here", "HEAD~1"],
        );
        run_git(root, &["tag", "-a", "v2-alias", "-m", "alias", "v2"]);
        let repo = open(root);

        let res = tag(&repo, "v2").unwrap();
        assert_eq!(res.kind, TagKind::Annotated);
        assert_eq!(res.tag_id.as_deref(), Some(rev(root, "v2").as_str()));
        assert_eq!(res.target, rev(root, "HEAD~1"));
        assert_eq!(res.target_type, ObjectType::Commit);
        assert_eq!(res.peeled, rev(root, "v2^{}"));
        assert_eq!(res.message.as_deref(), Some("Release 2\n\nNotes here\n"));
        let tagger = res.tagger.unwrap();
        assert_eq!(tagger.email, "test@example.com");

        let nested = tag(&repo, "v2-alias").unwrap();
        assert_eq!(nested.target, rev(root, "v2"));
        assert_eq!(nested.target_type, ObjectType::Tag);
        assert_eq!(nested.peeled, rev(root, "HEAD~1"));
        assert_eq!(nested.peeled_type, ObjectType::Commit);
    }

    #[test]
    fn tag_of_a_tree_and_missing_tags() {
        let (dir, _) = fixture_repo();
        run_git(dir.path(), &["tag", "tree-tag", "HEAD^{tree}"]);
        let repo = open(dir.path());
        let res = tag(&repo, "tree-tag").unwrap();
        assert_eq!(res.target_type, ObjectType::Tree);
        assert_eq!(res.peeled_type, ObjectType::Tree);

        for name in ["nope", "main", "bad..name"] {
            let err = format!("{:#}", tag(&repo, name).unwrap_err());
            assert!(err.contains("no tag named"), "{name}: {err}");
        }
    }

    fn describe_args(rev: Option<&str>) -> DescribeArgs {
        DescribeArgs {
            rev: rev.map(Into::into),
            tags: false,
            long: false,
            first_parent: false,
        }
    }

    /// `main`: c1 ← c2 (lightweight v1) ← c3 (annotated v2) ← c4 ← c5, plus a merge from a side branch.
    fn tagged_history() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let commit = |msg: &str, date: &str| {
            write(root, &format!("{msg}.txt"), msg);
            run_git(root, &["add", "."]);
            run_git_with_env(
                root,
                &["commit", "-q", "-m", msg],
                &[("GIT_COMMITTER_DATE", date), ("GIT_AUTHOR_DATE", date)],
            );
        };
        commit("c3", "2026-01-03T00:00:00Z");
        run_git_with_env(
            root,
            &["tag", "-a", "v2", "-m", "v2"],
            &[("GIT_COMMITTER_DATE", "2026-01-03T00:00:00Z")],
        );
        commit("c4", "2026-01-04T00:00:00Z");
        run_git(root, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        commit("s1", "2026-01-05T00:00:00Z");
        run_git_with_env(
            root,
            &["tag", "-a", "side-tag", "-m", "side"],
            &[("GIT_COMMITTER_DATE", "2026-01-05T00:00:00Z")],
        );
        run_git(root, &["checkout", "-q", "main"]);
        run_git_with_env(
            root,
            &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
            &[("GIT_COMMITTER_DATE", "2026-01-06T00:00:00Z")],
        );
        let repo = open(root);
        (dir, repo)
    }

    fn git_describe(dir: &std::path::Path, extra: &[&str]) -> String {
        let mut args = vec!["describe", "--always"];
        args.extend_from_slice(extra);
        git_stdout(dir, &args).trim().to_owned()
    }

    /// A revision, the matching `git describe` flags, and how to set the same options on `DescribeArgs`.
    type DescribeCase = (&'static str, &'static [&'static str], fn(&mut DescribeArgs));

    #[test]
    fn describe_matches_git() {
        let (dir, repo) = tagged_history();
        let root = dir.path();
        // HEAD is nearer `side-tag` through the merge, but nearer `v2` along first parents; HEAD~3
        // carries only the lightweight `v1`.
        let cases: [DescribeCase; 7] = [
            ("HEAD", &[], |_| {}),
            ("HEAD~1", &[], |_| {}),
            ("v2", &[], |_| {}),
            ("HEAD~3", &[], |_| {}),
            ("HEAD~3", &["--tags"], |a| a.tags = true),
            ("v2", &["--long"], |a| a.long = true),
            ("HEAD", &["--first-parent"], |a| a.first_parent = true),
        ];
        for (rev, flags, configure) in cases {
            let mut args = describe_args(Some(rev));
            configure(&mut args);
            let res = describe(&repo, args).unwrap();
            let mut git_args = flags.to_vec();
            git_args.push(rev);
            assert_eq!(
                res.description,
                git_describe(root, &git_args),
                "{rev} {flags:?}"
            );
        }

        let exact = describe(&repo, describe_args(Some("v2"))).unwrap();
        assert!(exact.exact);
        assert_eq!((exact.tag.as_deref(), exact.depth), (Some("v2"), 0));
        assert_eq!(exact.id, rev(root, "v2^{}"));
    }

    #[test]
    fn describe_without_tags_falls_back_to_sha() {
        let (dir, repo) = fixture_repo();
        // The fixture's only tag is lightweight, so plain describe finds nothing.
        let res = describe(&repo, describe_args(None)).unwrap();
        assert_eq!(res.tag, None);
        assert_eq!((res.depth, res.exact), (0, false));
        assert_eq!(res.description, git_describe(dir.path(), &[]));
        let with_tags = describe(
            &repo,
            DescribeArgs {
                tags: true,
                ..describe_args(None)
            },
        )
        .unwrap();
        assert_eq!(with_tags.description, "v1");
        assert!(with_tags.exact);
    }

    #[test]
    fn describe_errors() {
        let (_dir, repo) = fixture_repo();
        assert!(describe(&repo, describe_args(Some("nope"))).is_err());
        let (_dir, empty) = empty_repo();
        let err = format!("{:#}", describe(&empty, describe_args(None)).unwrap_err());
        assert!(err.contains("no commits yet"), "{err}");
    }

    #[test]
    fn describe_args_default_to_plain_describe() {
        let parsed: DescribeArgs = serde_json::from_str("{}").unwrap();
        assert!(parsed.rev.is_none() && !parsed.tags && !parsed.long && !parsed.first_parent);
    }
}
