//! `gitrs-mcp`: a local-only Git MCP server built on gitoxide (`gix`).
//!
//! The binary (`main.rs`) parses the command line and serves [`GitServer`] over stdio. The library
//! exists so that fuzz targets can link against the parsing entry points in [`fuzzing`]; it isn't
//! meant as a general-purpose API.

mod repo;
mod server;
mod tools;

pub use repo::RepoHandle;
pub use server::{GitServer, Policy};

/// Entry points for the fuzz targets in `fuzz/`: everything that parses untrusted input from a tool
/// call. Each returns an error for bad input; a panic is a bug.
#[doc(hidden)]
pub mod fuzzing {
    pub use crate::tools::repo_path;

    /// A resource URI: the `?repo=` query, then the `gitrs://` address.
    pub fn parse_resource_uri(uri: &str) -> Result<(), String> {
        let (base, _repo) = crate::server::split_repo(uri)?;
        crate::tools::resources::Address::parse(base)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Whether a path component would be refused as a spelling of `.git`.
    pub fn is_git_dir_name(component: &[u8]) -> bool {
        crate::tools::files::is_git_dir_name(component)
    }

    /// A `since`/`until` date as `search_commits` accepts it.
    pub fn parse_date(input: &str) -> Result<(), String> {
        crate::tools::history::parse_date("since", Some(input))
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// A revision as every tool resolves it, in `repo`.
    pub fn resolve_revision(repo: &gix::Repository, spec: &str) -> Result<String, String> {
        crate::tools::resolve_object(repo, Some(spec))
            .map(|id| id.to_string())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::fuzzing::*;
    use crate::tools::testutil::fixture_repo;

    #[test]
    fn fuzzing_entry_points_accept_and_refuse() {
        assert!(repo_path("src/./lib.rs").is_ok() && repo_path("../x").is_err());
        assert!(parse_resource_uri("gitrs://commit/HEAD?repo=a").is_ok());
        assert!(parse_resource_uri("gitrs://commit/HEAD?x=1").is_err());
        assert!(parse_resource_uri("gitrs://nope").is_err());
        assert!(is_git_dir_name(b".GiT.") && !is_git_dir_name(b".github"));
        assert!(parse_date("2024-01-31").is_ok() && parse_date("not a date").is_err());
        let (_dir, repo) = fixture_repo();
        assert_eq!(resolve_revision(&repo, "HEAD").unwrap().len(), 40);
        assert!(resolve_revision(&repo, "nope").is_err());
    }
}
