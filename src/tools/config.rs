use anyhow::bail;
use gix::config::Source;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{page_limit, paginate};

const DEFAULT_CONFIG_LIMIT: usize = 100;
const MAX_CONFIG_LIMIT: usize = 1000;
/// Replaces secret values, so the model sees that a value exists without seeing it.
const REDACTED: &str = "<redacted>";

/// Where a configuration value comes from, as `git config --show-scope` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConfigScope {
    /// The system-wide file (e.g. `/etc/gitconfig`) or the one shipped with git.
    System,
    /// The user's files: `~/.gitconfig` or `$XDG_CONFIG_HOME/git/config`.
    Global,
    /// The repository's `.git/config`.
    Local,
    /// The current worktree's `config.worktree` (with `extensions.worktreeConfig`).
    Worktree,
    /// `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` or `GIT_CONFIG_PARAMETERS` (`git -c`).
    Command,
}

impl From<Source> for ConfigScope {
    fn from(source: Source) -> Self {
        match source {
            Source::GitInstallation | Source::System => Self::System,
            Source::Git | Source::User => Self::Global,
            Source::Local => Self::Local,
            Source::Worktree => Self::Worktree,
            Source::Env | Source::Cli | Source::Api | Source::EnvOverride => Self::Command,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConfigGetArgs {
    /// A full key such as `user.email` or `remote.origin.url`: every value set for it is returned.
    /// Section and name are case-insensitive; the subsection in the middle (`origin`) is not.
    pub key: Option<String>,
    /// A section such as `core`, `remote` (every remote) or `remote.origin`, to list all of its keys.
    /// Omit both `key` and `section` to list everything, like `git config --list`.
    pub section: Option<String>,
    /// Only return values from this scope (default: all scopes).
    pub scope: Option<ConfigScope>,
    /// Maximum entries to return (default 100, max 1000).
    pub limit: Option<usize>,
    /// Number of entries to skip first, for pagination.
    pub skip: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConfigEntry {
    /// Full key with the section and name in lowercase and the subsection as written, e.g. `remote.origin.url`.
    pub key: String,
    /// The value, or `null` for a key written without `=`, which git reads as boolean true.
    /// Secrets are replaced with `<redacted>`.
    pub value: Option<String>,
    /// Which configuration level the value comes from.
    pub scope: ConfigScope,
    /// File the value was read from; `null` for values from the environment.
    pub file: Option<String>,
    /// True when `value` was hidden because it may be a secret: anything under `credential`, keys whose
    /// name contains `password`, `passwd`, `token` or `secret`, `http.extraHeader`, and the password in a URL.
    pub redacted: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConfigGetResult {
    /// Matching entries in the order git reads them: system, global, local, worktree, then command.
    /// When a single-valued key is set more than once, the last entry is the one git uses.
    pub entries: Vec<ConfigEntry>,
    /// True if more entries exist past this page.
    pub truncated: bool,
}

/// Configuration values with their scope and file, like `git config --show-scope --show-origin`,
/// with secrets redacted.
pub fn config_get(repo: &gix::Repository, args: ConfigGetArgs) -> anyhow::Result<ConfigGetResult> {
    let filter = Filter::new(args.key.as_deref(), args.section.as_deref())?;
    let snapshot = repo.config_snapshot();
    let entries = snapshot
        .plumbing()
        .sections()
        .filter(|section| shown_by_git(section.meta().source))
        .filter(|section| args.scope.is_none_or(|s| s == section.meta().source.into()))
        .flat_map(|section| section_entries(&section, &filter))
        .map(anyhow::Ok);
    let limit = page_limit(args.limit, DEFAULT_CONFIG_LIMIT, MAX_CONFIG_LIMIT);
    let (entries, truncated) = paginate(entries, args.skip.unwrap_or(0), limit)?;
    Ok(ConfigGetResult { entries, truncated })
}

/// Whether `git config --list` would show values from `source`. gix also turns environment variables
/// such as `GIT_EDITOR` or `HTTPS_PROXY` into config values (some under its own `gitoxide.*` keys);
/// git doesn't list those, so neither do we.
fn shown_by_git(source: Source) -> bool {
    source != Source::EnvOverride
}

/// The entries of one config section that `filter` selects, in file order.
fn section_entries(
    section: &gix::config::file::SectionRef<'_>,
    filter: &Filter,
) -> Vec<ConfigEntry> {
    let header = section.header();
    let name = header.name().to_string().to_ascii_lowercase();
    let subsection = header.subsection_name().map(|s| s.to_string());
    let meta = section.meta();
    let body = section.body();
    body.into_iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .filter(|(key, _)| filter.matches(&name, subsection.as_deref(), key))
        .map(|(key, value)| {
            // gix yields an empty value both for `key =` (the empty string, false to git) and a bare
            // `key` (true); only `value_implicit` tells them apart. It inspects the key's last
            // occurrence, which is exact unless one section mixes both forms of the same key.
            let implicit = value.is_empty() && body.value_implicit(&key) == Some(None);
            let (value, redacted) = if implicit {
                (None, false)
            } else {
                redact(&name, &key, value.to_string())
            };
            ConfigEntry {
                key: match &subsection {
                    Some(sub) => format!("{name}.{sub}.{key}"),
                    None => format!("{name}.{key}"),
                },
                value,
                scope: meta.source.into(),
                file: meta.path.as_ref().map(|p| p.display().to_string()),
                redacted,
            }
        })
        .collect()
}

/// Which entries a call asked for.
enum Filter {
    All,
    Section {
        name: String,
        subsection: Option<String>,
    },
    Key {
        name: String,
        subsection: Option<String>,
        key: String,
    },
}

impl Filter {
    fn new(key: Option<&str>, section: Option<&str>) -> anyhow::Result<Self> {
        match (key, section) {
            (Some(_), Some(_)) => bail!("pass either `key` or `section`, not both"),
            (Some(key), None) => {
                let (rest, name) = key.rsplit_once('.').filter(|(rest, name)| {
                    !rest.is_empty() && !name.is_empty()
                }).ok_or_else(|| {
                    anyhow::anyhow!(
                        "`key` must look like `section.name` or `section.subsection.name`, e.g. `user.email`; \
                         use `section` to list a whole section"
                    )
                })?;
                let (name_part, subsection) = split_section(rest);
                Ok(Self::Key {
                    name: name_part,
                    subsection,
                    key: name.to_ascii_lowercase(),
                })
            }
            (None, Some(section)) => {
                let (name, subsection) = split_section(section);
                Ok(Self::Section { name, subsection })
            }
            (None, None) => Ok(Self::All),
        }
    }

    fn matches(&self, name: &str, subsection: Option<&str>, key: &str) -> bool {
        match self {
            Self::All => true,
            Self::Section {
                name: want,
                subsection: want_sub,
            } => want == name && (want_sub.is_none() || want_sub.as_deref() == subsection),
            Self::Key {
                name: want,
                subsection: want_sub,
                key: want_key,
            } => want == name && want_sub.as_deref() == subsection && want_key == key,
        }
    }
}

/// `remote.origin` → (`remote`, `Some("origin")`). The section name is case-insensitive, the
/// subsection isn't; a subsection may itself contain dots (`url.https://example.com/`).
fn split_section(section: &str) -> (String, Option<String>) {
    match section.split_once('.') {
        Some((name, sub)) => (name.to_ascii_lowercase(), Some(sub.to_owned())),
        None => (section.to_ascii_lowercase(), None),
    }
}

/// The value to show and whether it was redacted.
fn redact(section: &str, key: &str, value: String) -> (Option<String>, bool) {
    if is_secret_key(section, key) {
        return (Some(REDACTED.to_owned()), true);
    }
    match redact_url_password(&value) {
        Some(clean) => (Some(clean), true),
        None => (Some(value), false),
    }
}

fn is_secret_key(section: &str, key: &str) -> bool {
    section == "credential"
        || ["password", "passwd", "token", "secret"]
            .iter()
            .any(|word| key.contains(word))
        || (section == "http" && key == "extraheader")
}

/// `url` with the password in `scheme://user:password@host/...` replaced, or `None` when it has none.
pub fn redact_url_password(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let valid_scheme = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    if !valid_scheme {
        return None;
    }
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    // The host follows the last `@`, so a password containing an unencoded `@` is still hidden whole.
    let (userinfo, host) = authority.rsplit_once('@')?;
    let (user, _password) = userinfo.split_once(':')?;
    Some(format!("{scheme}://{user}:{REDACTED}@{host}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{bare_repo, fixture_repo, git_stdout, open, run_git};

    fn args(key: Option<&str>, section: Option<&str>) -> ConfigGetArgs {
        ConfigGetArgs {
            key: key.map(Into::into),
            section: section.map(Into::into),
            scope: None,
            limit: None,
            skip: None,
        }
    }

    /// `(scope, key, value)` rows from `git config --list --show-scope`, the oracle for config_get.
    fn git_config(dir: &std::path::Path, extra: &[&str]) -> Vec<(String, String, Option<String>)> {
        let mut cmd = vec!["config", "--show-scope", "-z"];
        cmd.extend_from_slice(extra);
        git_stdout(dir, &cmd)
            .split('\0')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .chunks(2)
            .map(|pair| {
                let (key, value) = match pair[1].split_once('\n') {
                    Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
                    None => (pair[1].to_owned(), None),
                };
                (pair[0].to_owned(), key, value)
            })
            .collect()
    }

    fn rows(res: &ConfigGetResult) -> Vec<(String, String, Option<String>)> {
        res.entries
            .iter()
            .map(|e| {
                let scope = serde_json::to_value(e.scope).unwrap();
                (
                    scope.as_str().unwrap().to_owned(),
                    e.key.clone(),
                    e.value.clone(),
                )
            })
            .collect()
    }

    fn configured_repo() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let set = |k: &str, v: &str| run_git(root, &["config", k, v]);
        set("user.name", "Ada");
        set("remote.origin.url", "https://example.com/repo.git");
        set("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*");
        set("remote.Fork.url", "../fork");
        set("branch.main.remote", "origin");
        run_git(
            root,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/tags/*:refs/tags/*",
            ],
        );
        let repo = open(root);
        (dir, repo)
    }

    #[test]
    fn listing_matches_git_config() {
        let (dir, repo) = configured_repo();
        let res = config_get(&repo, args(None, None)).unwrap();
        assert_eq!(rows(&res), git_config(dir.path(), &["--list"]));
        assert!(
            res.entries
                .iter()
                .all(|e| e.scope == ConfigScope::Local && !e.redacted)
        );
        let file = res.entries[0].file.as_deref().unwrap();
        assert!(file.ends_with(".git/config"), "{file}");
        assert!(!res.truncated);
    }

    #[test]
    fn keys_and_sections_match_git_config() {
        let (dir, repo) = configured_repo();
        let root = dir.path();
        let res = config_get(&repo, args(Some("remote.origin.fetch"), None)).unwrap();
        assert_eq!(
            rows(&res),
            git_config(root, &["--get-regexp", r"^remote\.origin\.fetch$"])
        );
        assert_eq!(res.entries.len(), 2);

        // Section and key names are case-insensitive; the subsection is not.
        let res = config_get(&repo, args(Some("USER.Name"), None)).unwrap();
        assert_eq!(res.entries[0].value.as_deref(), Some("Ada"));
        assert!(
            config_get(&repo, args(Some("remote.fork.url"), None))
                .unwrap()
                .entries
                .is_empty()
        );
        assert_eq!(
            config_get(&repo, args(Some("remote.Fork.url"), None))
                .unwrap()
                .entries
                .len(),
            1
        );

        let section = config_get(&repo, args(None, Some("remote.origin"))).unwrap();
        assert_eq!(
            rows(&section),
            git_config(root, &["--get-regexp", r"^remote\.origin\."])
        );
        let remotes = config_get(&repo, args(None, Some("remote"))).unwrap();
        assert_eq!(
            rows(&remotes),
            git_config(root, &["--get-regexp", r"^remote\."])
        );
        assert_eq!(remotes.entries.len(), 4);
    }

    #[test]
    fn secrets_are_redacted() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let set = |k: &str, v: &str| run_git(root, &["config", k, v]);
        set("credential.helper", "store");
        set("http.extraHeader", "Authorization: Bearer abc");
        set("github.token", "ghp_secret");
        set("smtp.smtpPassword", "hunter2");
        set(
            "remote.origin.url",
            "https://user:s3cret@example.com/repo.git",
        );
        set("remote.mirror.url", "https://user@example.com/repo.git");
        let repo = open(root);
        let res = config_get(&repo, args(None, None)).unwrap();
        let get = |key: &str| res.entries.iter().find(|e| e.key == key).unwrap();
        for key in [
            "credential.helper",
            "http.extraheader",
            "github.token",
            "smtp.smtppassword",
        ] {
            assert_eq!(get(key).value.as_deref(), Some(REDACTED), "{key}");
            assert!(get(key).redacted, "{key}");
        }
        let url = get("remote.origin.url");
        assert_eq!(
            url.value.as_deref(),
            Some("https://user:<redacted>@example.com/repo.git")
        );
        assert!(url.redacted);
        assert!(!get("remote.mirror.url").redacted);
        let rendered = serde_json::to_string(&res).unwrap();
        for secret in ["abc", "ghp_secret", "hunter2", "s3cret"] {
            assert!(!rendered.contains(secret), "{secret} leaked");
        }
    }

    #[test]
    fn implicit_values_scope_filter_and_paging() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        let config = root.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[x]\n\tflag\n\tempty =\n");
        std::fs::write(&config, text).unwrap();
        let repo = open(root);
        let flag = config_get(&repo, args(Some("x.flag"), None)).unwrap();
        assert_eq!(flag.entries.len(), 1);
        assert_eq!(flag.entries[0].value, None);
        let section = config_get(&repo, args(None, Some("x"))).unwrap();
        assert_eq!(rows(&section), git_config(root, &["--get-regexp", r"^x\."]));

        let mut a = args(None, None);
        a.scope = Some(ConfigScope::Global);
        assert!(
            config_get(&repo, a).unwrap().entries.is_empty(),
            "tests run without global config"
        );

        let mut a = args(None, Some("core"));
        a.limit = Some(1);
        a.skip = Some(1);
        let page = config_get(&repo, a).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(page.truncated);
    }

    #[test]
    fn argument_errors() {
        let (_dir, repo) = fixture_repo();
        for key in ["user", ".name", "user."] {
            let err = format!(
                "{:#}",
                config_get(&repo, args(Some(key), None)).unwrap_err()
            );
            assert!(err.contains("section.name"), "{key}: {err}");
        }
        let err = format!(
            "{:#}",
            config_get(&repo, args(Some("a.b"), Some("a"))).unwrap_err()
        );
        assert!(err.contains("not both"), "{err}");
        let (_dir, bare) = bare_repo();
        let res = config_get(&bare, args(Some("core.bare"), None)).unwrap();
        assert_eq!(res.entries[0].value.as_deref(), Some("true"));
    }

    #[test]
    fn environment_overrides_are_not_listed() {
        assert!(!shown_by_git(Source::EnvOverride));
        for source in [
            Source::System,
            Source::User,
            Source::Local,
            Source::Worktree,
            Source::Env,
            Source::Cli,
        ] {
            assert!(shown_by_git(source), "{source:?}");
        }
    }

    #[test]
    fn url_password_redaction() {
        assert_eq!(
            redact_url_password("ssh://git:pw@host/x").as_deref(),
            Some("ssh://git:<redacted>@host/x")
        );
        assert_eq!(redact_url_password("https://host/x"), None);
        assert_eq!(redact_url_password("git@github.com:a/b.git"), None);
        assert_eq!(redact_url_password("../relative"), None);
        assert_eq!(
            redact_url_password("https://u:p@ss@host").as_deref(),
            Some("https://u:<redacted>@host")
        );
        assert_eq!(
            redact_url_password("https://user@host/a:b@c"),
            None,
            "only the authority counts"
        );
        assert_eq!(redact_url_password("1x://u:p@host"), None);
    }
}
