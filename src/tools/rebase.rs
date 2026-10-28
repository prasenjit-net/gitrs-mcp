use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, bail};
use gix::{
    ObjectId,
    index::entry::Stage,
    refs::{
        FullName, Target,
        transaction::{PreviousValue, RefEdit},
    },
    status::plumbing::index_as_worktree::EntryStatus,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    checkout::{
        RestoreRequest, apply_restore, apply_switch, plan_restore, plan_switch, tree_changes,
        updated_files,
    },
    commit::write_tree,
    diff::index_changes,
    integrate::{
        Merged, OperationResult, OperationStatus, RebaseProgress, apply_merged, move_head,
        new_commit, plan_merged, remove_state, restore_touched, with_conflicts, write_state,
    },
    resolve_commit,
    write::{append_reflog, clean_message, committer, ensure_idle, require_worktree, update_index},
};

/// git's state directory for the merge backend; ours is laid out the same way so git can take over.
const STATE_DIR: &str = "rebase-merge";
/// Messages given for `reword` and `squash` steps, by commit; git ignores this file.
const MESSAGES: &str = "gitrs-messages.json";
/// Files describing a step that stopped on conflicts.
const STOP_FILES: [&str; 3] = ["REBASE_HEAD", "MERGE_MSG", "AUTO_MERGE"];

/// What to do with one commit of the rebase plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TodoAction {
    /// Replay the commit as it is.
    Pick,
    /// Replay the commit with a new `message`.
    Reword,
    /// Fold the commit into the previous one, combining messages (or using `message`).
    Squash,
    /// Fold the commit into the previous one, keeping the previous message.
    Fixup,
    /// Leave the commit out.
    Drop,
}

impl TodoAction {
    fn word(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "pick" | "p" => Self::Pick,
            "reword" | "r" => Self::Reword,
            "squash" | "s" => Self::Squash,
            "fixup" | "f" => Self::Fixup,
            "drop" | "d" => Self::Drop,
            _ => return None,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoStep {
    /// What to do with the commit.
    pub action: TodoAction,
    /// The commit, e.g. a SHA or `feature~1`.
    pub commit: String,
    /// New message for `reword`, or the combined message for `squash` (default: both messages).
    pub message: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RebaseArgs {
    /// Commit to replay the current branch onto, e.g. `main`.
    pub onto: String,
    /// Replay the branch's commits that aren't in this revision (default: `onto`), like
    /// `git rebase --onto <onto> <upstream>`.
    pub upstream: Option<String>,
    /// The plan to follow instead of picking every commit in order, like an edited `git rebase -i`
    /// todo list. Steps run in the order given.
    pub todo: Option<Vec<TodoStep>>,
}

/// Replay the current branch's commits onto another commit, like `git rebase`.
pub fn rebase(repo: &gix::Repository, args: RebaseArgs) -> anyhow::Result<OperationResult> {
    let workdir = require_worktree(repo, "rebase")?.to_owned();
    ensure_idle(repo, "rebase")?;
    committer(repo)?;
    let branch = repo
        .head_name()?
        .context("HEAD is detached; switch to the branch you want to rebase first")?;
    let head = repo.head_id()?.detach();
    let head_tree = repo.head_tree_id()?.detach();
    ensure_clean(repo, head_tree)?;
    let onto = resolve_commit(repo, Some(&args.onto))?;
    let upstream = resolve_commit(repo, Some(args.upstream.as_deref().unwrap_or(&args.onto)))?.id;
    // Like git, a branch that already sits on top of `onto` has nothing to replay.
    if args.todo.is_none()
        && upstream == onto.id
        && repo
            .merge_base(onto.id, head)?
            .is_some_and(|base| base == onto.id)
    {
        return OperationResult::new(repo, OperationStatus::UpToDate);
    }

    let mut messages = BTreeMap::new();
    let steps: Vec<(TodoAction, ObjectId)> = match args.todo {
        Some(todo) => todo
            .into_iter()
            .map(|step| {
                let id = resolve_commit(repo, Some(&step.commit))?.id;
                if let Some(message) = step.message {
                    messages.insert(id.to_string(), message);
                }
                if step.action == TodoAction::Reword && !messages.contains_key(&id.to_string()) {
                    bail!("`reword` of `{}` needs a `message`", step.commit);
                }
                Ok((step.action, id))
            })
            .collect::<anyhow::Result<_>>()?,
        None => {
            let mut picks = Vec::new();
            for info in repo.rev_walk([head]).with_hidden([upstream]).all()? {
                let commit = info?.object()?;
                // Like git, merge commits are left out rather than replayed.
                if commit.parent_ids().count() <= 1 {
                    picks.push((TodoAction::Pick, commit.id));
                }
            }
            picks.reverse();
            picks
        }
    };
    if steps.is_empty() && onto.id == head {
        return OperationResult::new(repo, OperationStatus::UpToDate);
    }
    if let Some((TodoAction::Squash | TodoAction::Fixup, _)) = steps.first() {
        bail!(
            "the first step can't be `squash` or `fixup`: there is no previous commit to fold into"
        );
    }

    let mut todo = String::new();
    for (action, id) in &steps {
        let subject = repo.find_commit(*id)?.message()?.summary().to_string();
        todo.push_str(&format!("{} {id} # {subject}\n", action.word()));
    }
    let dir = state_dir(repo);
    std::fs::create_dir_all(&dir)?;
    for (name, content) in [
        ("head-name", format!("{}\n", branch.as_bstr())),
        ("onto", format!("{}\n", onto.id)),
        ("orig-head", format!("{head}\n")),
        ("git-rebase-todo", todo),
        ("done", String::new()),
        ("msgnum", "0\n".to_owned()),
        ("end", format!("{}\n", steps.len())),
        ("interactive", String::new()),
        (MESSAGES, serde_json::to_string(&messages)?),
    ] {
        std::fs::write(dir.join(name), content)?;
    }
    write_state(repo, "ORIG_HEAD", &format!("{head}\n"))?;

    // Detach HEAD at `onto`, carrying the (clean) working tree along.
    let changes = tree_changes(repo, head_tree, onto.tree_id()?.detach())?;
    let action = format!("rebasing onto `{}`", args.onto);
    update_index(repo, |index| {
        let steps = plan_switch(repo, index, &workdir, &changes, &action)?;
        apply_switch(repo, index, &workdir, steps)?;
        repo.edit_reference(
            RefEdit::update(
                "HEAD".try_into()?,
                onto.id,
                PreviousValue::MustExistAndMatch(Target::Symbolic(branch.clone())),
                format!("rebase (start): checkout {}", args.onto),
            )
            .with_deref(false),
        )?;
        Ok(())
    })?;
    drive(repo, &workdir)
}

/// How to resume a stopped rebase.
#[derive(Clone, Copy)]
pub enum Resume {
    /// Commit the resolved step, then carry on.
    Continue,
    /// Leave out the step that stopped, then carry on.
    Skip,
    /// Go back to the branch as it was before the rebase.
    Abort,
}

/// Continue, skip or abort the rebase in progress, whether gitrs-mcp or git started it.
pub fn resume(repo: &gix::Repository, how: Resume) -> anyhow::Result<OperationResult> {
    let workdir = require_worktree(repo, "rebase")?.to_owned();
    let dir = state_dir(repo);
    if !dir.is_dir() {
        bail!(
            "no rebase is in progress here (or it uses git's older `rebase-apply` backend); use git"
        );
    }
    match how {
        Resume::Abort => abort(repo, &workdir),
        Resume::Skip => {
            if stopped_commit(repo)?.is_some() {
                restore_touched(repo, &workdir, repo.head_tree_id()?.detach())?;
                clear_stop(repo)?;
            }
            drive(repo, &workdir)
        }
        Resume::Continue => {
            let index = repo.index_or_empty()?;
            if index
                .entries()
                .iter()
                .any(|e| e.stage() != Stage::Unconflicted)
            {
                bail!(
                    "there are still unresolved conflicts; resolve and `stage` them first (`conflicts` lists them)"
                );
            }
            if let Some(picked) = stopped_commit(repo)? {
                let action = last_done_action(repo)?;
                let tree = write_tree(repo, &index)?;
                let author = read_author(&dir)?.map_or_else(
                    || Ok::<_, anyhow::Error>(repo.find_commit(picked)?.author()?.to_owned()?),
                    Ok,
                )?;
                let message = message_for(repo, picked)?
                    .or_else(|| {
                        let text = std::fs::read_to_string(dir.join("message")).ok()?;
                        let kept: Vec<&str> =
                            text.lines().filter(|l| !l.starts_with('#')).collect();
                        clean_message(&kept.join("\n"))
                    })
                    .context("the stopped step has no message; continue with git")?;
                commit_step(repo, action, tree, author, &message, picked, "continue")?;
                clear_stop(repo)?;
            }
            drive(repo, &workdir)
        }
    }
}

/// Replay steps until the plan is done (then finish) or a step conflicts (then stop).
fn drive(repo: &gix::Repository, workdir: &Path) -> anyhow::Result<OperationResult> {
    let dir = state_dir(repo);
    let mut last = None;
    loop {
        let todo = std::fs::read_to_string(dir.join("git-rebase-todo")).unwrap_or_default();
        let mut lines = todo
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
        let Some(line) = lines.next() else {
            return finish(repo, last);
        };
        let rest: String = lines.map(|l| format!("{l}\n")).collect();
        let mut words = line.split_whitespace();
        let word = words.next().unwrap_or_default();
        let action = TodoAction::parse(word).with_context(|| {
            format!(
                "the rebase plan has a `{word}` step, which gitrs-mcp can't run; continue with git"
            )
        })?;
        let picked = ObjectId::from_hex(
            words
                .next()
                .context("a rebase step names no commit")?
                .as_bytes(),
        )?;
        std::fs::write(dir.join("git-rebase-todo"), rest)?;
        append_line(&dir.join("done"), line)?;
        let msgnum: usize = std::fs::read_to_string(dir.join("msgnum"))
            .ok()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);
        std::fs::write(dir.join("msgnum"), format!("{}\n", msgnum + 1))?;
        if action == TodoAction::Drop {
            continue;
        }

        let commit = repo.find_commit(picked)?;
        let parents: Vec<ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
        if parents.len() > 1 {
            bail!(
                "the rebase plan picks the merge commit {picked}, which gitrs-mcp can't replay; use git"
            );
        }
        let base = match parents.first() {
            Some(parent) => repo.find_commit(*parent)?.tree_id()?.detach(),
            None => ObjectId::empty_tree(repo.object_hash()),
        };
        let head_tree = repo.head_tree_id()?.detach();
        let label = format!("{} ({})", commit.short_id()?, commit.message()?.summary());
        let theirs = commit.tree_id()?.detach();
        let merged = Merged::trees(repo, base, head_tree, theirs, &label)?;
        let step_label = format!("replaying {}", commit.short_id()?);
        let updated = update_index(repo, |index| {
            let steps = plan_merged(repo, index, workdir, head_tree, &merged, &step_label)?;
            apply_merged(repo, index, workdir, steps, &merged)
        })?;
        let author = commit.author()?.to_owned()?;
        let original = commit.message_raw()?.to_string();
        if !merged.is_clean() {
            let message = with_conflicts(&original, &merged);
            std::fs::write(dir.join("stopped-sha"), format!("{picked}\n"))?;
            std::fs::write(dir.join("message"), &message)?;
            std::fs::write(dir.join("author-script"), author_script(&author))?;
            write_state(repo, "REBASE_HEAD", &format!("{picked}\n"))?;
            write_state(repo, "MERGE_MSG", &message)?;
            let mut result =
                OperationResult::new(repo, OperationStatus::Stopped)?.with_files(updated, &merged);
            result.rebase = Some(progress(repo, Some(picked))?);
            return Ok(result);
        }
        let message = message_for(repo, picked)?.unwrap_or(original);
        last = commit_step(
            repo,
            action,
            merged.tree,
            author,
            &message,
            picked,
            action.word(),
        )?
        .or(last);
    }
}

/// Record one replayed step on the detached HEAD. A pick that changes nothing is dropped, as git does.
fn commit_step(
    repo: &gix::Repository,
    action: TodoAction,
    tree: ObjectId,
    author: gix::actor::Signature,
    message: &str,
    picked: ObjectId,
    reflog: &str,
) -> anyhow::Result<Option<ObjectId>> {
    let head = repo.head_commit()?;
    let head_id = head.id;
    let (parents, author, message) = match action {
        TodoAction::Squash | TodoAction::Fixup => {
            let previous = head.message_raw()?.to_string();
            let message = match (action, message_for(repo, picked)?) {
                (TodoAction::Squash, Some(given)) => given,
                (TodoAction::Squash, None) => format!("{}\n\n{message}", previous.trim_end()),
                _ => previous,
            };
            let parents = head.parent_ids().map(|id| id.detach()).collect();
            (parents, head.author()?.to_owned()?, message)
        }
        _ => {
            if tree == head.tree_id()?.detach() {
                return Ok(None);
            }
            (vec![head_id], author, message.to_owned())
        }
    };
    let message = clean_message(&message).context("a rebase step would have an empty message")?;
    let commit = new_commit(repo, tree, parents, Some(author), &message)?;
    move_head(
        repo,
        head_id,
        commit,
        &format!(
            "rebase ({reflog}): {}",
            message.lines().next().unwrap_or_default()
        ),
    )?;
    Ok(Some(commit))
}

/// Point the branch at the rewritten commits and put HEAD back on it.
fn finish(repo: &gix::Repository, last: Option<ObjectId>) -> anyhow::Result<OperationResult> {
    let dir = state_dir(repo);
    let branch = head_name(repo)?;
    let orig = read_id(&dir.join("orig-head"))?;
    let onto = read_id(&dir.join("onto"))?;
    let head = repo.head_id()?.detach();
    if head != orig {
        repo.edit_reference(RefEdit::update(
            branch.clone(),
            head,
            PreviousValue::MustExistAndMatch(Target::Object(orig)),
            format!("rebase (finish): {} onto {onto}", branch.as_bstr()),
        ))?;
    }
    reattach(
        repo,
        &branch,
        head,
        &format!("rebase (finish): returning to {}", branch.as_bstr()),
    )?;
    let total = progress(repo, None)?;
    std::fs::remove_dir_all(&dir)?;
    remove_state(repo, &STOP_FILES);
    let mut result = OperationResult::new(repo, OperationStatus::Finished)?
        .with_commit(repo, last.or(Some(head)))?;
    result.rebase = Some(total);
    Ok(result)
}

/// Go back to the original branch tip, restoring the index and working tree to it.
fn abort(repo: &gix::Repository, workdir: &Path) -> anyhow::Result<OperationResult> {
    let dir = state_dir(repo);
    let branch = head_name(repo)?;
    let orig = read_id(&dir.join("orig-head"))?;
    let everything = [".".to_owned()];
    let source = orig.to_string();
    let request = RestoreRequest {
        paths: &everything,
        source: &source,
        staged: true,
        worktree: true,
        overwrite_conflicts: true,
    };
    let updated = update_index(repo, |index| {
        let plan = plan_restore(repo, index, workdir, &request)?;
        apply_restore(repo, index, workdir, &plan)?;
        Ok(updated_files(&plan))
    })?;
    let head = repo.head_id()?.detach();
    reattach(
        repo,
        &branch,
        head,
        &format!("rebase (abort): returning to {}", branch.as_bstr()),
    )?;
    std::fs::remove_dir_all(&dir)?;
    remove_state(repo, &STOP_FILES);
    Ok(OperationResult::new(repo, OperationStatus::Aborted)?.with_updated(updated))
}

/// Point HEAD back at `branch` (from the detached commit `from`) and log it, as git does.
fn reattach(
    repo: &gix::Repository,
    branch: &FullName,
    from: ObjectId,
    message: &str,
) -> anyhow::Result<()> {
    repo.edit_reference(
        RefEdit::update(
            "HEAD".try_into()?,
            Target::Symbolic(branch.clone()),
            PreviousValue::MustExistAndMatch(Target::Object(from)),
            message,
        )
        .with_deref(false),
    )?;
    let to = repo.head_id()?.detach();
    append_reflog(repo, "HEAD", Some(from), to, message)
}

/// Refuse to start a rebase with local changes to tracked files, as git does.
fn ensure_clean(repo: &gix::Repository, head_tree: ObjectId) -> anyhow::Result<()> {
    let (staged, unmerged) = index_changes(repo, &head_tree, &[])?;
    if !staged.is_empty() || !unmerged.is_empty() {
        bail!("cannot rebase: the index has staged changes; commit or stash them first");
    }
    let status = repo
        .status(gix::progress::Discard)?
        .untracked_files(gix::status::UntrackedFiles::None)
        .into_index_worktree_iter(Vec::<gix::bstr::BString>::new())?;
    for item in status {
        if let gix::status::index_worktree::Item::Modification {
            status, rela_path, ..
        } = item?
            && !matches!(status, EntryStatus::NeedsUpdate(_))
        {
            bail!("cannot rebase: `{rela_path}` has unstaged changes; commit or stash them first");
        }
    }
    Ok(())
}

fn state_dir(repo: &gix::Repository) -> std::path::PathBuf {
    repo.git_dir().join(STATE_DIR)
}

fn head_name(repo: &gix::Repository) -> anyhow::Result<FullName> {
    let text = std::fs::read_to_string(state_dir(repo).join("head-name"))?;
    let name = text.trim();
    if name == "detached HEAD" {
        bail!("this rebase started on a detached HEAD, which gitrs-mcp doesn't handle; use git");
    }
    Ok(FullName::try_from(name)?)
}

fn read_id(path: &Path) -> anyhow::Result<ObjectId> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(ObjectId::from_hex(text.trim().as_bytes())?)
}

fn append_line(path: &Path, line: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// The commit a stopped step was replaying, if the rebase is stopped.
fn stopped_commit(repo: &gix::Repository) -> anyhow::Result<Option<ObjectId>> {
    let dir = state_dir(repo);
    for path in [dir.join("stopped-sha"), repo.git_dir().join("REBASE_HEAD")] {
        if path.is_file() {
            return read_id(&path).map(Some);
        }
    }
    Ok(None)
}

fn clear_stop(repo: &gix::Repository) -> anyhow::Result<()> {
    let dir = state_dir(repo);
    for name in ["stopped-sha", "message", "author-script"] {
        std::fs::remove_file(dir.join(name)).ok();
    }
    remove_state(repo, &STOP_FILES);
    Ok(())
}

fn last_done_action(repo: &gix::Repository) -> anyhow::Result<TodoAction> {
    let done = std::fs::read_to_string(state_dir(repo).join("done")).unwrap_or_default();
    let word = done
        .lines()
        .last()
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("pick");
    TodoAction::parse(word).with_context(|| {
        format!("the stopped step is a `{word}`, which gitrs-mcp can't continue; use git")
    })
}

/// The message given for `picked` in this rebase's plan, if any.
fn message_for(repo: &gix::Repository, picked: ObjectId) -> anyhow::Result<Option<String>> {
    let Ok(text) = std::fs::read_to_string(state_dir(repo).join(MESSAGES)) else {
        return Ok(None);
    };
    let messages: BTreeMap<String, String> = serde_json::from_str(&text)?;
    Ok(messages.get(&picked.to_string()).cloned())
}

fn progress(
    repo: &gix::Repository,
    stopped_at: Option<ObjectId>,
) -> anyhow::Result<RebaseProgress> {
    let dir = state_dir(repo);
    let done = std::fs::read_to_string(dir.join("done"))
        .unwrap_or_default()
        .lines()
        .count();
    let total = std::fs::read_to_string(dir.join("end"))
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(done);
    Ok(RebaseProgress {
        done,
        total,
        stopped_at: stopped_at.map(|id| id.to_string()),
    })
}

/// git's `author-script`: shell assignments of the stopped commit's author.
fn author_script(author: &gix::actor::Signature) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let offset = author.time.offset;
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.abs() / 60;
    format!(
        "GIT_AUTHOR_NAME={}\nGIT_AUTHOR_EMAIL={}\nGIT_AUTHOR_DATE={}\n",
        quote(&author.name.to_string()),
        quote(&author.email.to_string()),
        quote(&format!(
            "@{} {sign}{:02}{:02}",
            author.time.seconds,
            minutes / 60,
            minutes % 60
        )),
    )
}

/// The author recorded in `author-script`, written by us or by git.
fn read_author(dir: &Path) -> anyhow::Result<Option<gix::actor::Signature>> {
    let Ok(text) = std::fs::read_to_string(dir.join("author-script")) else {
        return Ok(None);
    };
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let value = value
                .trim()
                .trim_start_matches('\'')
                .trim_end_matches('\'')
                .replace("'\\''", "'");
            fields.insert(key.to_owned(), value);
        }
    }
    let (Some(name), Some(email), Some(date)) = (
        fields.get("GIT_AUTHOR_NAME"),
        fields.get("GIT_AUTHOR_EMAIL"),
        fields.get("GIT_AUTHOR_DATE"),
    ) else {
        return Ok(None);
    };
    let time = gix::date::parse(date.trim_start_matches('@'), None)
        .or_else(|_| gix::date::parse(date, None))
        .with_context(|| format!("cannot read the author date `{date}` from author-script"))?;
    Ok(Some(gix::actor::Signature {
        name: name.as_str().into(),
        email: email.as_str().into(),
        time,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::tools::{
        ChangeKind,
        integrate::{ContinueArgs, abort_operation, continue_operation, tests::updated},
        testutil::{
            assert_unchanged, diverged, git_stdout, open, pinned_git as git, pinned_twins,
            porcelain, snapshot, try_git, worktree_files, write,
        },
    };

    fn onto(repo: &gix::Repository, target: &str) -> anyhow::Result<OperationResult> {
        rebase(
            repo,
            RebaseArgs {
                onto: target.into(),
                upstream: None,
                todo: None,
            },
        )
    }

    fn on_feature(conflict: bool) -> impl Fn(&Path) {
        move |root: &Path| {
            diverged(root, conflict);
            git(root, &["checkout", "-q", "feature"]);
        }
    }

    /// Trees, subjects and authors of the last `n` commits, which a rewritten history must match.
    fn history(root: &Path, n: usize) -> String {
        git_stdout(root, &["log", &format!("-{n}"), "--format=%T %an %ad %s"])
    }

    fn reflog(root: &Path, name: &str, n: usize) -> String {
        git_stdout(
            root,
            &["reflog", "show", &format!("-{n}"), "--format=%gs", name],
        )
    }

    fn cont() -> ContinueArgs {
        ContinueArgs {
            message: None,
            skip: false,
        }
    }

    fn refused<T: std::fmt::Debug>(
        root: &Path,
        needle: &str,
        f: impl FnOnce() -> anyhow::Result<T>,
    ) {
        let before = snapshot(root);
        let err = format!("{:#}", f().unwrap_err());
        assert!(err.contains(needle), "expected `{needle}` in: {err}");
        assert_unchanged(root, &before);
    }

    #[test]
    fn clean_rebase_matches_git() {
        let [(ours, repo), (theirs, _)] = pinned_twins(on_feature(false));
        let res = onto(&repo, "main").unwrap();
        git(theirs.path(), &["rebase", "-q", "main"]);
        assert_eq!(res.status, OperationStatus::Finished);
        assert_eq!(res.branch.as_deref(), Some("feature"));
        let progress = res.rebase.unwrap();
        assert_eq!((progress.done, progress.total), (2, 2));
        assert_eq!(history(ours.path(), 4), history(theirs.path(), 4));
        assert_eq!(worktree_files(ours.path()), worktree_files(theirs.path()));
        assert_eq!(porcelain(ours.path()), porcelain(theirs.path()));
        assert_eq!(
            reflog(ours.path(), "HEAD", 4),
            reflog(theirs.path(), "HEAD", 4)
        );
        assert_eq!(
            reflog(ours.path(), "feature", 1),
            reflog(theirs.path(), "feature", 1)
        );
        assert!(!ours.path().join(".git/rebase-merge").exists());
        let up_to_date = onto(&repo, "main").unwrap();
        assert_eq!(up_to_date.status, OperationStatus::UpToDate);
    }

    #[test]
    fn stopped_rebase_writes_gits_state_and_git_can_finish_it() {
        let [(ours, repo), (theirs, _)] = pinned_twins(on_feature(true));
        let res = onto(&repo, "main").unwrap();
        assert!(!try_git(theirs.path(), &["rebase", "-q", "main"]));
        assert_eq!(res.status, OperationStatus::Stopped);
        assert_eq!(res.conflicts, ["a"]);
        let state = |r: &Path, f: &str| std::fs::read_to_string(r.join(".git").join(f)).unwrap();
        for file in [
            "rebase-merge/head-name",
            "rebase-merge/onto",
            "rebase-merge/orig-head",
            "rebase-merge/git-rebase-todo",
            "rebase-merge/done",
            "rebase-merge/msgnum",
            "rebase-merge/end",
            "rebase-merge/stopped-sha",
            "rebase-merge/message",
            "rebase-merge/author-script",
            "REBASE_HEAD",
            "MERGE_MSG",
        ] {
            assert_eq!(
                state(ours.path(), file),
                state(theirs.path(), file),
                "{file}"
            );
        }
        assert_eq!(worktree_files(ours.path()), worktree_files(theirs.path()));
        assert_eq!(porcelain(ours.path()), porcelain(theirs.path()));

        // git continues what we started; we continue what git started.
        for root in [ours.path(), theirs.path()] {
            write(root, "a", "resolved\n");
            git(root, &["add", "a"]);
        }
        git(ours.path(), &["rebase", "--continue"]);
        let res = continue_operation(&open(theirs.path()), cont()).unwrap();
        assert_eq!(res.status, OperationStatus::Finished);
        assert_eq!(history(ours.path(), 4), history(theirs.path(), 4));
        assert_eq!(
            reflog(ours.path(), "HEAD", 4),
            reflog(theirs.path(), "HEAD", 4)
        );
        assert_eq!(porcelain(theirs.path()), "");
    }

    #[test]
    fn abort_and_skip() {
        let [(ours, repo), (theirs, _)] = pinned_twins(on_feature(true));
        let before = worktree_files(ours.path());
        let tip = git_stdout(ours.path(), &["rev-parse", "feature"]);
        onto(&repo, "main").unwrap();
        let res = abort_operation(&repo).unwrap();
        assert_eq!(res.status, OperationStatus::Aborted);
        assert_eq!(
            updated(&res),
            [
                ("a".to_owned(), ChangeKind::Modified),
                ("c".to_owned(), ChangeKind::Added)
            ]
        );
        assert_eq!(worktree_files(ours.path()), before);
        assert_eq!(git_stdout(ours.path(), &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            git_stdout(ours.path(), &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/feature"
        );
        assert!(!ours.path().join(".git/rebase-merge").exists());
        assert_eq!(
            reflog(ours.path(), "HEAD", 1).trim(),
            "rebase (abort): returning to refs/heads/feature"
        );

        // git aborts our rebase.
        onto(&repo, "main").unwrap();
        git(ours.path(), &["rebase", "--abort"]);
        assert_eq!(worktree_files(ours.path()), before);

        // Skipping the conflicting commit matches `git rebase --skip`.
        onto(&repo, "main").unwrap();
        let skipped = continue_operation(
            &repo,
            ContinueArgs {
                message: None,
                skip: true,
            },
        )
        .unwrap();
        assert!(!try_git(theirs.path(), &["rebase", "-q", "main"]));
        git(theirs.path(), &["rebase", "--skip"]);
        assert_eq!(skipped.status, OperationStatus::Finished);
        assert_eq!(history(ours.path(), 3), history(theirs.path(), 3));
    }

    #[test]
    fn todo_plans() {
        let [(dir, repo), _] = pinned_twins(on_feature(false));
        let root = dir.path();
        write(root, "d", "dee\n");
        git(root, &["add", "d"]);
        git(root, &["commit", "-q", "-m", "add d"]);
        let step = |action, commit: &str, message: Option<&str>| TodoStep {
            action,
            commit: commit.into(),
            message: message.map(Into::into),
        };
        let res = rebase(
            &repo,
            RebaseArgs {
                onto: "main".into(),
                upstream: None,
                todo: Some(vec![
                    step(TodoAction::Reword, "feature~2", Some("Edit a on feature")),
                    step(TodoAction::Fixup, "feature~1", None),
                    step(TodoAction::Drop, "feature", None),
                ]),
            },
        )
        .unwrap();
        assert_eq!(res.status, OperationStatus::Finished);
        let log = git_stdout(root, &["log", "-3", "--format=%s"]);
        assert_eq!(log, "Edit a on feature\nmain edit\nbase\n");
        assert!(root.join("c").exists(), "the fixup folded `add c` in");
        assert!(!root.join("d").exists(), "`add d` was dropped");

        let squashed = rebase(
            &repo,
            RebaseArgs {
                onto: "main~1".into(),
                upstream: Some("main".into()),
                todo: Some(vec![
                    step(TodoAction::Pick, "HEAD", None),
                    step(TodoAction::Squash, "feature~1", Some("One commit")),
                ]),
            },
        );
        // `feature~1` is `main edit` now, which the squash folds onto the reworded commit.
        assert_eq!(squashed.unwrap().status, OperationStatus::Finished);
        assert_eq!(
            git_stdout(root, &["log", "-1", "--format=%s"]),
            "One commit\n"
        );
    }

    #[test]
    fn commits_already_upstream_are_dropped_like_git() {
        let setup = |root: &Path| {
            diverged(root, false);
            git(root, &["cherry-pick", "feature"]);
            git(root, &["checkout", "-q", "feature"]);
        };
        let [(ours, repo), (theirs, _)] = pinned_twins(setup);
        onto(&repo, "main").unwrap();
        git(theirs.path(), &["rebase", "-q", "main"]);
        assert_eq!(history(ours.path(), 4), history(theirs.path(), 4));
    }

    #[test]
    fn plans_with_steps_we_cannot_run_are_left_to_git() {
        let [(dir, repo), _] = pinned_twins(on_feature(true));
        let root = dir.path();
        assert!(!try_git(root, &["rebase", "-q", "main"]));
        let todo = root.join(".git/rebase-merge/git-rebase-todo");
        let mut text = std::fs::read_to_string(&todo).unwrap();
        text.insert_str(0, "exec make test\n");
        std::fs::write(&todo, text).unwrap();
        write(root, "a", "resolved\n");
        git(root, &["add", "a"]);
        let err = continue_operation(&repo, cont()).unwrap_err().to_string();
        assert!(err.contains("`exec` step"), "{err}");
    }

    #[test]
    fn rebase_refusals_change_nothing() {
        let [(dir, repo), _] = pinned_twins(on_feature(false));
        let root = dir.path();
        write(root, "a", "dirty\n");
        refused(root, "unstaged changes", || onto(&repo, "main"));
        git(root, &["checkout", "--", "a"]);
        refused(root, "cannot resolve", || onto(&repo, "nope"));
        let reword = rebase(
            &repo,
            RebaseArgs {
                onto: "main".into(),
                upstream: None,
                todo: Some(vec![TodoStep {
                    action: TodoAction::Reword,
                    commit: "HEAD".into(),
                    message: None,
                }]),
            },
        );
        assert!(
            reword
                .unwrap_err()
                .to_string()
                .contains("needs a `message`")
        );
        let first_squash = rebase(
            &repo,
            RebaseArgs {
                onto: "main".into(),
                upstream: None,
                todo: Some(vec![TodoStep {
                    action: TodoAction::Fixup,
                    commit: "HEAD".into(),
                    message: None,
                }]),
            },
        );
        assert!(first_squash.unwrap_err().to_string().contains("first step"));
        git(root, &["checkout", "-q", "--detach"]);
        refused(root, "detached", || onto(&repo, "main"));
    }
}
