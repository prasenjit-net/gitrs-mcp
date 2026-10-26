use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::fixture_repo;

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
}
