//! The repositories this server serves: those named with `--repo`, plus repositories at the client's
//! MCP roots. Every tool, prompt and resource takes an optional `repo` that picks one of them.

use std::{
    path::{Path, PathBuf},
    sync::RwLock,
};

use percent_encoding::percent_decode_str;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::repo::RepoHandle;

/// Arguments of a tool, plus the repository to run it on.
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct InRepo<T> {
    /// Which served repository to use: a name from `list_repos`, or a path inside one. Default: the
    /// first repository the server was started with.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(flatten)]
    pub args: T,
}

/// The arguments of a tool that only needs to know which repository to use.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct RepoOnly {
    /// Which served repository to use: a name from `list_repos`, or a path inside one. Default: the
    /// first repository the server was started with.
    #[serde(default)]
    pub repo: Option<String>,
}

/// Where a served repository came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// Named with `--repo` when the server started.
    CommandLine,
    /// A repository at one of the client's MCP roots.
    ClientRoots,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ServedRepo {
    /// Name to pass as `repo`: the root directory's name, with `-2`, `-3`, ... when names repeat.
    pub name: String,
    /// Absolute path of the repository's root: its working tree, or the git directory if bare.
    pub root: String,
    /// True for the repository used when `repo` is omitted.
    pub default: bool,
    /// True for a bare repository, which has no working tree.
    pub bare: bool,
    /// Where it came from: `command_line` or `client_roots`.
    pub source: Source,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ListReposResult {
    /// Every served repository; the default comes first.
    pub repos: Vec<ServedRepo>,
}

struct Served {
    name: String,
    handle: RepoHandle,
    source: Source,
}

/// What a call gets when the server started outside a repository and the client has no roots.
const NO_REPOSITORY: &str = "no repository is being served: open a folder inside a git repository \
     in your client, or add `--repo <path>` to this server's registration";

/// The served repositories. The first one is the default; it always comes from the command line.
pub(crate) struct Repos {
    list: RwLock<Vec<Served>>,
}

impl Repos {
    pub fn new(command_line: Vec<RepoHandle>) -> Self {
        let repos = Self {
            list: RwLock::new(Vec::new()),
        };
        repos.add(command_line, Source::CommandLine);
        repos
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Served>> {
        self.list
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Served>> {
        self.list
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Add repositories not served yet, naming each after its root directory.
    fn add(&self, handles: Vec<RepoHandle>, source: Source) {
        let mut list = self.write();
        for handle in handles {
            if list.iter().any(|s| s.handle.root() == handle.root()) {
                continue;
            }
            let base = handle
                .root()
                .file_name()
                .map_or_else(|| "repo".to_owned(), |n| n.to_string_lossy().into_owned());
            let mut name = base.clone();
            for n in 2.. {
                if !list.iter().any(|s| s.name == name) {
                    break;
                }
                name = format!("{base}-{n}");
            }
            list.push(Served {
                name,
                handle,
                source,
            });
        }
    }

    /// Serve exactly `handles` from the client's roots, replacing the previous roots.
    pub fn set_roots(&self, handles: Vec<RepoHandle>) {
        self.write().retain(|s| s.source != Source::ClientRoots);
        self.add(handles, Source::ClientRoots);
    }

    /// The repository `repo` names: a served repository's name, or a path inside one (the most
    /// specific when they nest). Anything else is refused, so a client can't reach other paths.
    pub fn select(&self, repo: Option<&str>) -> Result<RepoHandle, String> {
        let list = self.read();
        let Some(wanted) = repo.filter(|r| !r.is_empty()) else {
            return list
                .first()
                .map(|s| s.handle.clone())
                .ok_or_else(|| NO_REPOSITORY.to_owned());
        };
        if let Some(served) = list.iter().find(|s| s.name == wanted) {
            return Ok(served.handle.clone());
        }
        let path = resolve(Path::new(wanted));
        list.iter()
            .filter(|s| {
                path.as_ref()
                    .is_some_and(|p| p.starts_with(s.handle.root()))
            })
            .max_by_key(|s| s.handle.root().as_os_str().len())
            .map(|s| s.handle.clone())
            .ok_or_else(|| {
                let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
                format!(
                    "`{wanted}` isn't a served repository; pass one of {} (list_repos shows them)",
                    names.join(", ")
                )
            })
    }

    /// Each served repository's name and handle, the default first.
    pub fn entries(&self) -> Vec<(String, RepoHandle)> {
        self.read()
            .iter()
            .map(|s| (s.name.clone(), s.handle.clone()))
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.read().iter().map(|s| s.name.clone()).collect()
    }

    pub fn is_single(&self) -> bool {
        self.read().len() <= 1
    }

    pub fn describe(&self) -> ListReposResult {
        let repos = self
            .read()
            .iter()
            .enumerate()
            .map(|(i, s)| ServedRepo {
                name: s.name.clone(),
                root: s.handle.root().display().to_string(),
                default: i == 0,
                bare: s.handle.is_bare(),
                source: s.source,
            })
            .collect();
        ListReposResult { repos }
    }
}

/// `path` with symlinks resolved as far as it exists, so it compares with the canonical roots even
/// when it names something not created yet. `None` for relative paths and paths with `..`.
fn resolve(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    let (existing, rest) = path.ancestors().find_map(|dir| {
        Some((
            std::fs::canonicalize(dir).ok()?,
            path.strip_prefix(dir).ok()?,
        ))
    })?;
    Some(existing.join(rest))
}

/// The repositories at the client's roots: each `file://` root that is, or is inside, a repository
/// whose root lies within it. A repository above a root isn't served, since that would reach paths
/// the client didn't declare.
pub(crate) fn repos_at_roots(uris: &[String]) -> Vec<RepoHandle> {
    uris.iter()
        .filter_map(|uri| {
            let found = root_repo(uri);
            if let Err(err) = &found {
                tracing::info!(%uri, %err, "client root not served");
            }
            found.ok()
        })
        .collect()
}

fn root_repo(uri: &str) -> anyhow::Result<RepoHandle> {
    let path = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow::anyhow!("only file:// roots can be served"))?;
    let path = PathBuf::from(percent_decode_str(path).decode_utf8()?.as_ref());
    let declared = std::fs::canonicalize(&path)?;
    let root = RepoHandle::find_root(&declared)?;
    if !root.starts_with(&declared) {
        anyhow::bail!(
            "the repository at {} reaches outside the root {}",
            root.display(),
            declared.display()
        );
    }
    RepoHandle::open(root)
}

/// Split the `?repo=<name>` query off a resource URI.
pub(crate) fn split_repo(uri: &str) -> Result<(&str, Option<String>), String> {
    let Some((base, query)) = uri.split_once('?') else {
        return Ok((uri, None));
    };
    let repo = query
        .strip_prefix("repo=")
        .filter(|r| !r.contains('&'))
        .ok_or_else(|| format!("`{uri}`: the only query a gitrs URI takes is `?repo=<name>`"))?;
    let repo = percent_decode_str(repo)
        .decode_utf8()
        .map_err(|_| format!("`{uri}`: `repo` isn't valid percent-encoded UTF-8"))?;
    Ok((base, Some(repo.into_owned())))
}

/// What a repository name can't contain unencoded in a `?repo=` query.
const QUERY_UNSAFE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'=')
    .add(b'?');

/// `uri` pointed at `repo`, unless that's the default.
pub(crate) fn with_repo(uri: &str, repo: Option<&str>) -> String {
    match repo {
        Some(name) => format!(
            "{uri}?repo={}",
            percent_encoding::utf8_percent_encode(name, QUERY_UNSAFE)
        ),
        None => uri.to_owned(),
    }
}

/// The `file://` URI of a directory, for tests building roots.
#[cfg(test)]
pub(crate) fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{fixture_repo, run_git};

    fn handle(path: &std::path::Path) -> RepoHandle {
        RepoHandle::open_isolated(path)
    }

    #[test]
    fn repositories_are_named_and_selected() {
        let (a, _) = fixture_repo();
        let (b, _) = fixture_repo();
        let nested = a.path().join("vendor/lib");
        std::fs::create_dir_all(&nested).unwrap();
        run_git(&nested, &["init", "-q"]);
        let repos = Repos::new(vec![handle(a.path()), handle(b.path()), handle(a.path())]);
        assert_eq!(repos.entries().len(), 2, "the same root is served once");

        // A second root with the same directory name gets a suffix.
        let twin_dir = tempfile::tempdir().unwrap();
        let twin = twin_dir.path().join(a.path().file_name().unwrap());
        std::fs::create_dir(&twin).unwrap();
        run_git(&twin, &["init", "-q"]);
        repos.set_roots(vec![handle(&twin), handle(&nested)]);
        let names = repos.names();
        let a_name = a.path().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(names[0], a_name);
        assert_eq!(names[2], format!("{a_name}-2"));
        assert_eq!(names[3], "lib");

        let root = |r: Result<RepoHandle, String>| r.unwrap().root().to_owned();
        let canonical = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(root(repos.select(None)), canonical(a.path()));
        assert_eq!(root(repos.select(Some(""))), canonical(a.path()));
        assert_eq!(root(repos.select(Some(&names[1]))), canonical(b.path()));
        let inside = a.path().join("a.txt");
        assert_eq!(
            root(repos.select(Some(inside.to_str().unwrap()))),
            canonical(a.path())
        );
        let deep = nested.join("x/y");
        assert_eq!(
            root(repos.select(Some(deep.to_str().unwrap()))),
            canonical(&nested),
            "the most specific repository wins"
        );
        let err = repos.select(Some("/")).err().unwrap();
        assert!(
            err.contains("isn't a served repository") && err.contains(&a_name),
            "{err}"
        );
        let sneaky = format!("{}/../../etc", a.path().display());
        for refused in ["a.txt", sneaky.as_str()] {
            assert!(repos.select(Some(refused)).is_err(), "{refused}");
        }

        let described = repos.describe().repos;
        assert!(described[0].default && !described[1].default);
        assert_eq!(described[3].source, Source::ClientRoots);
        assert!(!repos.is_single());

        repos.set_roots(Vec::new());
        assert_eq!(
            repos.names().len(),
            2,
            "roots are replaced, the command line stays"
        );
    }

    #[test]
    fn client_roots_serve_repositories_inside_them_only() {
        let (dir, _) = fixture_repo();
        let sub = dir.path().join("sub dir");
        std::fs::create_dir(&sub).unwrap();
        let encoded = format!("file://{}", dir.path().display()).replace(' ', "%20");
        let uris = [
            encoded,
            file_uri(&sub),
            "https://example.com/repo".to_owned(),
            file_uri(&dir.path().join("missing")),
        ];
        let served = repos_at_roots(&uris);
        assert_eq!(
            served.len(),
            1,
            "only the root that is the repository itself"
        );
        assert_eq!(served[0].root(), std::fs::canonicalize(dir.path()).unwrap());
        let err = root_repo(&file_uri(&sub)).err().unwrap().to_string();
        assert!(err.contains("reaches outside the root"), "{err}");
    }

    #[test]
    fn repo_queries_split_off_resource_uris() {
        assert_eq!(
            split_repo("gitrs://commit/HEAD").unwrap(),
            ("gitrs://commit/HEAD", None)
        );
        assert_eq!(
            split_repo("gitrs://commit/HEAD?repo=my%20repo").unwrap(),
            ("gitrs://commit/HEAD", Some("my repo".to_owned()))
        );
        for bad in [
            "gitrs://commit/HEAD?rev=x",
            "gitrs://commit/HEAD?repo=a&b=c",
            "x?repo=%FF",
        ] {
            assert!(split_repo(bad).is_err(), "{bad}");
        }
        assert_eq!(
            with_repo("gitrs://commit/HEAD", None),
            "gitrs://commit/HEAD"
        );
        assert_eq!(
            with_repo("gitrs://commit/HEAD", Some("my repo.git")),
            "gitrs://commit/HEAD?repo=my%20repo.git"
        );
        assert_eq!(
            with_repo("gitrs://commit/HEAD", Some("a&b")),
            "gitrs://commit/HEAD?repo=a%26b"
        );
    }

    proptest::proptest! {
        /// Any repository name survives `?repo=` encoding and decoding.
        #[test]
        fn repo_queries_round_trip(name in "\\PC{1,16}") {
            let uri = with_repo("gitrs://commit/HEAD", Some(&name));
            let (base, repo) = split_repo(&uri).unwrap();
            proptest::prop_assert_eq!(base, "gitrs://commit/HEAD");
            proptest::prop_assert_eq!(repo, Some(name));
        }
    }
}
