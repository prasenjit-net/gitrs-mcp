use std::path::Path;

use anyhow::bail;
use gix::{
    attrs::StateRef,
    bstr::{BStr, BString, ByteSlice},
    index::entry::Mode,
    worktree::stack::state::{attributes, ignore},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::repo_file_path;

/// Paths per call; each one is a separate lookup, and the answer lists every path.
const MAX_PATHS: usize = 1000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckIgnoreArgs {
    /// Paths to check, relative to the repository root (at most 1000). They don't have to exist. A path
    /// that exists as a directory, or ends with `/`, is checked as a directory.
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IgnoreCheck {
    /// The path as checked, relative to the repository root (a trailing `/` is kept).
    pub path: String,
    /// True when git ignores the path: a rule matches it or a parent directory, the rule isn't a
    /// `!` negation, and the path isn't tracked.
    pub ignored: bool,
    /// True when the path is in the index. git never ignores tracked files, whatever the rules say.
    pub tracked: bool,
    /// The deciding rule as written, e.g. `*.log`, `target/` or `!keep.log` (a negation re-includes the
    /// path); `null` when no rule matches. Reported for tracked paths too.
    pub pattern: Option<String>,
    /// File the rule comes from: relative to the repository root when inside it (`.gitignore`,
    /// `src/.gitignore`, `.git/info/exclude`), otherwise absolute (`core.excludesFile`). `null` when no rule matches.
    pub source: Option<String>,
    /// 1-based line of the rule in `source`; `null` when no rule matches.
    pub line: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckIgnoreResult {
    /// One answer per requested path, in request order.
    pub paths: Vec<IgnoreCheck>,
}

/// Whether paths are ignored and by which rule, like `git check-ignore -v --non-matching`.
pub fn check_ignore(
    repo: &gix::Repository,
    args: CheckIgnoreArgs,
) -> anyhow::Result<CheckIgnoreResult> {
    let targets = targets(repo, &args.paths)?;
    let index = repo.index_or_empty()?;
    let mut stack = repo.excludes(
        &index,
        None,
        ignore::Source::WorktreeThenIdMappingIfNotSkipped,
    )?;
    let paths = targets
        .into_iter()
        .map(|target| {
            let rule = deciding_rule(&mut stack, repo, target.path.as_bstr(), target.is_dir)?;
            let tracked = index.entry_by_path(target.path.as_bstr()).is_some();
            let excludes = rule.as_ref().is_some_and(|r| !r.negative);
            Ok(IgnoreCheck {
                path: target.display,
                ignored: excludes && !tracked,
                tracked,
                line: rule.as_ref().map(|r| r.line),
                source: rule.as_ref().and_then(|r| r.source.clone()),
                pattern: rule.map(|r| r.pattern),
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(CheckIgnoreResult { paths })
}

/// An ignore rule, detached from the stack that found it.
struct Rule {
    pattern: String,
    negative: bool,
    source: Option<String>,
    line: usize,
}

/// The rule git reports for `path`. As in git, once a parent directory is excluded everything below
/// it is too, and no rule further down (not even a negation) can re-include it.
fn deciding_rule(
    stack: &mut gix::AttributeStack<'_>,
    repo: &gix::Repository,
    path: &BStr,
    is_dir: bool,
) -> anyhow::Result<Option<Rule>> {
    let parents = path
        .char_indices()
        .filter(|(_, _, c)| *c == '/')
        .map(|(start, _, _)| &path[..start]);
    for parent in parents {
        let rule = matching_rule(stack, repo, parent.as_bstr(), true)?;
        if rule.as_ref().is_some_and(|r| !r.negative) {
            return Ok(rule);
        }
    }
    matching_rule(stack, repo, path, is_dir)
}

fn matching_rule(
    stack: &mut gix::AttributeStack<'_>,
    repo: &gix::Repository,
    path: &BStr,
    is_dir: bool,
) -> anyhow::Result<Option<Rule>> {
    let mode = if is_dir { Mode::DIR } else { Mode::FILE };
    let platform = stack.at_entry(path, Some(mode))?;
    Ok(platform.matching_exclude_pattern().map(|m| Rule {
        pattern: m.pattern.to_string(),
        negative: m.pattern.is_negative(),
        source: m.source.map(|s| display_source(repo, s)),
        line: m.sequence_number,
    }))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckAttrArgs {
    /// Paths to check, relative to the repository root (at most 1000). They don't have to exist.
    pub paths: Vec<String>,
    /// Attribute names to look up, e.g. `["text", "diff", "linguist-generated"]`. Omit or pass `[]` to
    /// get every attribute set on each path, like `git check-attr -a`.
    #[serde(default)]
    pub attrs: Vec<String>,
}

/// How an attribute applies to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttrState {
    /// Set, e.g. `text` in `*.c text`.
    Set,
    /// Unset, e.g. `-text`.
    Unset,
    /// Set to a value, e.g. `diff=rust`; the value is in `value`.
    Value,
    /// No rule mentions it (or `!attr` reset it). Only reported for attributes named in `attrs`.
    Unspecified,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AttrValue {
    /// Attribute name.
    pub name: String,
    /// Whether the attribute is set, unset, has a value, or is unspecified.
    pub state: AttrState,
    /// The value when `state` is `value`, e.g. `rust`; `null` otherwise.
    pub value: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PathAttrs {
    /// The path as checked, relative to the repository root.
    pub path: String,
    /// The requested attributes in request order, or with `attrs` empty, every attribute that is set,
    /// unset or has a value (macros such as `binary` expand to `diff`, `merge` and `text`).
    pub attributes: Vec<AttrValue>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckAttrResult {
    /// One answer per requested path, in request order.
    pub paths: Vec<PathAttrs>,
}

/// Git attributes of paths from `.gitattributes`, `.git/info/attributes` and `core.attributesFile`,
/// like `git check-attr`.
pub fn check_attr(repo: &gix::Repository, args: CheckAttrArgs) -> anyhow::Result<CheckAttrResult> {
    let targets = targets(repo, &args.paths)?;
    let index = repo.index_or_empty()?;
    // Without a worktree, `.gitattributes` can only come from the index, as with `git check-attr --cached`.
    let source = if repo.workdir().is_some() {
        attributes::Source::WorktreeThenIdMapping
    } else {
        attributes::Source::IdMapping
    };
    let mut stack = repo.attributes_only(&index, source)?;
    let mut outcome = if args.attrs.is_empty() {
        stack.attribute_matches()
    } else {
        stack.selected_attribute_matches(args.attrs.iter().map(String::as_str))
    };
    let paths = targets
        .into_iter()
        .map(|target| {
            outcome.reset();
            let mode = if target.is_dir { Mode::DIR } else { Mode::FILE };
            stack
                .at_entry(target.path.as_bstr(), Some(mode))?
                .matching_attributes(&mut outcome);
            let attributes = if args.attrs.is_empty() {
                outcome
                    .iter()
                    .filter(|m| m.assignment.state != StateRef::Unspecified)
                    .map(|m| attr_value(m.assignment))
                    .collect()
            } else {
                outcome
                    .iter_selected()
                    .map(|m| attr_value(m.assignment))
                    .collect()
            };
            Ok(PathAttrs {
                path: target.display,
                attributes,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(CheckAttrResult { paths })
}

fn attr_value(assignment: gix::attrs::AssignmentRef<'_>) -> AttrValue {
    let (state, value) = match assignment.state {
        StateRef::Set => (AttrState::Set, None),
        StateRef::Unset => (AttrState::Unset, None),
        StateRef::Value(v) => (AttrState::Value, Some(v.as_bstr().to_string())),
        StateRef::Unspecified => (AttrState::Unspecified, None),
    };
    AttrValue {
        name: assignment.name.as_str().to_owned(),
        state,
        value,
    }
}

/// A path to look up.
struct Target {
    /// Normalized and relative to the repository root, without a trailing `/`.
    path: BString,
    /// Checked as a directory: given with a trailing `/`, or a directory in the worktree. A symlink to
    /// a directory counts as a file, as in git.
    is_dir: bool,
    /// The path as reported back: normalized, keeping a trailing `/` the caller gave.
    display: String,
}

/// Normalized lookup targets, refusing an empty list, too many paths, and paths escaping the repository.
fn targets(repo: &gix::Repository, paths: &[String]) -> anyhow::Result<Vec<Target>> {
    if paths.is_empty() {
        bail!("`paths` is empty; pass at least one path relative to the repository root");
    }
    if paths.len() > MAX_PATHS {
        bail!(
            "`paths` has {} entries; pass at most {MAX_PATHS} per call",
            paths.len()
        );
    }
    paths
        .iter()
        .map(|given| {
            let path = repo_file_path(given)?;
            let slash = given.ends_with('/');
            Ok(Target {
                is_dir: slash || is_worktree_dir(repo, path.as_bstr()),
                display: if slash {
                    format!("{path}/")
                } else {
                    path.to_string()
                },
                path,
            })
        })
        .collect()
}

fn is_worktree_dir(repo: &gix::Repository, path: &BStr) -> bool {
    repo.workdir().is_some_and(|workdir| {
        gix::path::from_bstr(path)
            .is_ok_and(|rel| std::fs::symlink_metadata(workdir.join(rel)).is_ok_and(|m| m.is_dir()))
    })
}

/// A rule's source file relative to the worktree when it's inside, otherwise as an absolute path.
fn display_source(repo: &gix::Repository, source: &Path) -> String {
    repo.workdir()
        .and_then(|workdir| {
            let workdir = workdir.canonicalize().ok()?;
            let source = source.canonicalize().ok()?;
            source.strip_prefix(workdir).ok().map(Path::to_path_buf)
        })
        .unwrap_or_else(|| source.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{bare_repo, fixture_repo, git_output, open, run_git, write};

    fn ignore(repo: &gix::Repository, paths: &[&str]) -> anyhow::Result<CheckIgnoreResult> {
        check_ignore(
            repo,
            CheckIgnoreArgs {
                paths: paths.iter().map(|p| p.to_string()).collect(),
            },
        )
    }

    fn attrs(repo: &gix::Repository, paths: &[&str], names: &[&str]) -> CheckAttrResult {
        check_attr(
            repo,
            CheckAttrArgs {
                paths: paths.iter().map(|p| p.to_string()).collect(),
                attrs: names.iter().map(|n| n.to_string()).collect(),
            },
        )
        .unwrap()
    }

    /// NUL-separated fields from a `git ... -z` command, in groups of `width`. `stdin` paths are sent
    /// NUL-separated, as `check-ignore -z --stdin` requires.
    fn git_z(dir: &Path, args: &[&str], stdin: &[&str], width: usize) -> Vec<Vec<String>> {
        let input: String = stdin.iter().map(|p| format!("{p}\0")).collect();
        let out = git_output(dir, args, input.as_bytes());
        // check-ignore exits 1 when nothing is ignored.
        assert!(out.status.code().unwrap() <= 1, "git {args:?} failed");
        let text = String::from_utf8(out.stdout).unwrap();
        let fields: Vec<_> = text.split('\0').map(str::to_owned).collect();
        fields[..fields.len() - 1]
            .chunks(width)
            .map(<[String]>::to_vec)
            .collect()
    }

    /// Ignore rules at the root, in a subdirectory and in `.git/info/exclude`, plus a tracked `*.log` file.
    fn ignore_fixture() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(
            root,
            ".gitignore",
            "target/\n*.log\n!keep.log\n/build\ndocs/*.tmp\n",
        );
        write(root, "sub/.gitignore", "*.tmp\n");
        let exclude = root.join(".git/info/exclude");
        let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
        text.push_str("secret.txt\n");
        std::fs::write(&exclude, text).unwrap();
        write(root, "tracked.log", "x");
        run_git(
            root,
            &["add", "-f", ".gitignore", "sub/.gitignore", "tracked.log"],
        );
        run_git(root, &["commit", "-q", "-m", "ignore rules"]);
        let repo = open(root);
        (dir, repo)
    }

    const IGNORE_PATHS: [&str; 15] = [
        "target/foo.rs",
        "target",
        "target/keep.log",
        "debug.log",
        "keep.log",
        "build",
        "build/",
        "sub/a.tmp",
        "a.tmp",
        "docs/a.tmp",
        "docs/deep/a.tmp",
        "secret.txt",
        "tracked.log",
        "src/main.rs",
        "dïr/x.log",
    ];

    /// Our answers as `git check-ignore -v -n -z` rows: `[source, line, pattern, path]`.
    fn ignore_rows(res: &CheckIgnoreResult) -> Vec<Vec<String>> {
        res.paths
            .iter()
            .map(|p| {
                vec![
                    p.source.clone().unwrap_or_default(),
                    p.line.map(|l| l.to_string()).unwrap_or_default(),
                    p.pattern.clone().unwrap_or_default(),
                    p.path.clone(),
                ]
            })
            .collect()
    }

    #[test]
    fn rules_match_git_check_ignore() {
        let (dir, repo) = ignore_fixture();
        let root = dir.path();
        let res = ignore(&repo, &IGNORE_PATHS).unwrap();
        let git_args = ["check-ignore", "-v", "-n", "-z", "--stdin", "--no-index"];
        assert_eq!(ignore_rows(&res), git_z(root, &git_args, &IGNORE_PATHS, 4));

        // Without --no-index git leaves tracked files out; `ignored` must agree with it.
        let git_ignored: Vec<bool> = git_z(root, &git_args[..5], &IGNORE_PATHS, 4)
            .iter()
            .map(|row| !row[2].is_empty() && !row[2].starts_with('!'))
            .collect();
        let ours: Vec<bool> = res.paths.iter().map(|p| p.ignored).collect();
        assert_eq!(ours, git_ignored);

        let by_path = |path: &str| res.paths.iter().find(|p| p.path == path).unwrap();
        assert!(by_path("tracked.log").tracked && !by_path("tracked.log").ignored);
        assert_eq!(by_path("tracked.log").pattern.as_deref(), Some("*.log"));
        assert_eq!(by_path("keep.log").pattern.as_deref(), Some("!keep.log"));
        assert!(!by_path("keep.log").ignored);
        assert_eq!(
            by_path("target/keep.log").pattern.as_deref(),
            Some("target/")
        );
        assert_eq!(
            by_path("secret.txt").source.as_deref(),
            Some(".git/info/exclude")
        );
        assert_eq!(by_path("src/main.rs").pattern, None);
    }

    #[test]
    fn existing_directories_are_checked_as_directories() {
        let (dir, repo) = ignore_fixture();
        let root = dir.path();
        std::fs::create_dir(root.join("target")).unwrap();
        let res = ignore(&repo, &["target"]).unwrap();
        assert!(res.paths[0].ignored);
        assert_eq!(
            ignore_rows(&res),
            git_z(
                root,
                &["check-ignore", "-v", "-n", "-z", "--stdin"],
                &["target"],
                4
            )
        );
    }

    #[test]
    fn ignore_errors_and_bare_repositories() {
        let (_dir, repo) = ignore_fixture();
        let msg = |r: anyhow::Result<CheckIgnoreResult>| format!("{:#}", r.unwrap_err());
        assert!(msg(ignore(&repo, &[])).contains("`paths` is empty"));
        assert!(msg(ignore(&repo, &["../x"])).contains(".."));
        assert!(msg(ignore(&repo, &["/etc"])).contains("absolute"));
        let many: Vec<String> = (0..=MAX_PATHS).map(|n| format!("f{n}")).collect();
        let err = check_ignore(&repo, CheckIgnoreArgs { paths: many }).unwrap_err();
        assert!(err.to_string().contains("at most 1000"), "{err}");

        let (_dir, bare) = bare_repo();
        let res = ignore(&bare, &["x.log"]).unwrap();
        assert!(!res.paths[0].ignored);
    }

    fn attr_fixture() -> (tempfile::TempDir, gix::Repository) {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        write(
            root,
            ".gitattributes",
            "*.png binary\n*.rs diff=rust text eol=lf\ndocs/** -text\n[attr]myattrs foo bar=baz\n*.md myattrs\n*.txt !text\n",
        );
        write(root, "sub/.gitattributes", "*.rs linguist-generated\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "attributes"]);
        let repo = open(root);
        (dir, repo)
    }

    const ATTR_PATHS: [&str; 7] = [
        "img.png",
        "src/a.rs",
        "README.md",
        "docs/x.rs",
        "sub/b.rs",
        "a.txt",
        "dïr/ünï.rs",
    ];

    /// Our answers as `git check-attr -z` rows: `[path, attribute, value]`.
    fn attr_rows(res: &CheckAttrResult) -> Vec<Vec<String>> {
        res.paths
            .iter()
            .flat_map(|p| {
                p.attributes.iter().map(|a| {
                    let value = match a.state {
                        AttrState::Set => "set".to_owned(),
                        AttrState::Unset => "unset".to_owned(),
                        AttrState::Unspecified => "unspecified".to_owned(),
                        AttrState::Value => a.value.clone().unwrap(),
                    };
                    vec![p.path.clone(), a.name.clone(), value]
                })
            })
            .collect()
    }

    #[test]
    fn named_attributes_match_git_check_attr() {
        let (dir, repo) = attr_fixture();
        let names = ["text", "diff", "eol", "linguist-generated", "nothing"];
        let res = attrs(&repo, &ATTR_PATHS, &names);
        let mut git_args = vec!["check-attr", "-z"];
        git_args.extend_from_slice(&names);
        git_args.push("--");
        git_args.extend_from_slice(&ATTR_PATHS);
        assert_eq!(attr_rows(&res), git_z(dir.path(), &git_args, &[], 3));
        assert_eq!(res.paths[1].attributes[1].value.as_deref(), Some("rust"));
    }

    #[test]
    fn all_attributes_match_git_check_attr() {
        let (dir, repo) = attr_fixture();
        let res = attrs(&repo, &ATTR_PATHS, &[]);
        let mut git_args = vec!["check-attr", "-z", "-a", "--"];
        git_args.extend_from_slice(&ATTR_PATHS);
        // gix lists attributes in declaration order, git puts macros first; compare as sets.
        let mut ours = attr_rows(&res);
        let mut git = git_z(dir.path(), &git_args, &[], 3);
        ours.sort();
        git.sort();
        assert_eq!(ours, git);
        let png = &res.paths[0].attributes;
        assert!(
            png.iter()
                .any(|a| a.name == "binary" && a.state == AttrState::Set)
        );
        assert!(
            png.iter()
                .any(|a| a.name == "diff" && a.state == AttrState::Unset)
        );
    }

    #[test]
    fn attr_errors_and_bare_repositories() {
        let (_dir, repo) = attr_fixture();
        let err = check_attr(
            &repo,
            CheckAttrArgs {
                paths: vec![],
                attrs: vec![],
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("`paths` is empty"));

        // A bare repository has no worktree or index, so in-tree .gitattributes don't apply.
        let (_dir, bare) = bare_repo();
        let res = attrs(&bare, &["a.rs"], &["diff"]);
        assert_eq!(res.paths[0].attributes[0].state, AttrState::Unspecified);
    }

    #[test]
    fn args_defaults() {
        let parsed: CheckAttrArgs = serde_json::from_str(r#"{"paths":["a"]}"#).unwrap();
        assert!(parsed.attrs.is_empty());
    }
}
