//! `gitrs-mcp setup`: register this server with the AI agents on the machine.
//!
//! Orchestration only. What each agent needs lives in [`agents`], how a plan is worked out in
//! [`plan`], and how files are edited in [`json`], [`toml`] and [`files`]. This layer imports
//! neither `rmcp` nor the tools: it never touches a repository.

mod agents;
mod binary;
mod cli;
mod document;
mod env;
mod files;
mod json;
mod plan;
mod prompt;
mod toml;

use std::{io::IsTerminal, path::PathBuf};

use anyhow::Context;

use agents::Agent;
pub use agents::Client;
use env::Env;
use plan::{CliCall, Effect, Op, Plan, Verdict, Wanted};
use prompt::Terminal;

/// Where an entry is registered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    /// For you, in every project.
    User,
    /// In the current project's own file, to commit and share. Uses the bare command
    /// `gitrs-mcp`, which each teammate needs on their `PATH`.
    Project,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Register gitrs-mcp with AI agents (Claude Code, Claude Desktop, Cursor, VS Code, Codex CLI).
///
/// On a terminal it finds the agents you have, asks which to use, and shows every change before
/// making it. Elsewhere it never asks: name the agents with --client or --all. Files are parsed,
/// backed up and replaced atomically; a file that can't be parsed is never overwritten.
#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// Agent to register with. Repeat for several. Without it, a terminal gets a checklist and
    /// anything else must pass --all.
    #[arg(long = "client", value_enum, value_name = "AGENT")]
    clients: Vec<Client>,

    /// Use every agent found on this machine (with --remove: every agent setup knows).
    #[arg(long)]
    all: bool,

    /// Where to register. Default: user for installing; with --remove or --status, both.
    #[arg(long, value_enum)]
    scope: Option<Scope>,

    /// Repository the server should serve. Repeatable. Without it the server uses the client's
    /// project folder, which Claude Desktop doesn't have.
    #[arg(long = "repo", value_name = "PATH")]
    repos: Vec<PathBuf>,

    /// Register the server with its write tools enabled (stage, commit, branch, ...). Off unless
    /// you ask for it here or answer yes when prompted.
    #[arg(long)]
    allow_write: bool,

    /// Register the server with --watch, for resource subscriptions.
    #[arg(long)]
    watch: bool,

    /// Name of the entry in the agents' configuration.
    #[arg(long, default_value = "gitrs")]
    name: String,

    /// Show what would change and change nothing.
    #[arg(long)]
    dry_run: bool,

    /// Don't ask for confirmation.
    #[arg(long)]
    yes: bool,

    /// Replace an existing entry of the same name that differs from the one being registered.
    #[arg(long)]
    force: bool,

    /// Show where the server is registered, and whether its command still exists.
    #[arg(long, conflicts_with_all = ["remove", "print"])]
    status: bool,

    /// Remove the entry from the agents.
    #[arg(long, conflicts_with = "print")]
    remove: bool,

    /// Print the registration for one --client, to add by hand. Changes nothing.
    #[arg(long)]
    print: bool,
}

/// Run `gitrs-mcp setup` against this machine, on the real terminal.
pub fn run(args: SetupArgs) -> anyhow::Result<()> {
    let env = Env::from_process()?;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    execute(
        &args,
        &env,
        &mut Terminal {
            input: &mut input,
            out: &mut out,
        },
        interactive,
    )
}

fn execute(
    args: &SetupArgs,
    env: &Env,
    term: &mut Terminal,
    interactive: bool,
) -> anyhow::Result<()> {
    let mut wanted = Wanted {
        name: args.name.clone(),
        repos: args.repos.clone(),
        allow_write: args.allow_write,
        watch: args.watch,
        force: args.force,
    };
    if args.print {
        return print(args, &wanted, env, term);
    }
    let op = if args.status {
        Op::Status
    } else if args.remove {
        Op::Remove
    } else {
        Op::Add
    };
    let agents = pick_agents(args, op, env, term, interactive)?;
    if op == Op::Add && interactive && !args.yes && !args.allow_write && !agents.is_empty() {
        wanted.allow_write = term.confirm(
            "Enable write tools (stage, commit, branch, merge, ...)? Off keeps the server read-only.",
            false,
        )?;
    }
    let plans = plans_for(&agents, args, op, &wanted, env);
    report_and_apply(plans, args, op, &wanted, env, term, interactive)
}

/// `--print`: the registration for a single agent, for adding by hand.
fn print(args: &SetupArgs, wanted: &Wanted, env: &Env, term: &mut Terminal) -> anyhow::Result<()> {
    let [client] = args.clients.as_slice() else {
        anyhow::bail!("--print needs exactly one --client, for example `--client cursor`");
    };
    let agent = agents::get(*client);
    let scope = args.scope.unwrap_or(Scope::User);
    anyhow::ensure!(
        agent.supports(scope, env),
        "{} has no {} configuration setup can edit",
        agent.name,
        scope.label()
    );
    let place = match agent.file(scope, env) {
        Some(file) => format!("Add this to {}", file.display()),
        None => "Run this".to_owned(),
    };
    term.say(&format!(
        "{place} (format checked against {} {}):",
        agent.name, agent.verified
    ))?;
    term.say(plan::snippet(agent, scope, wanted, env).trim_end())
}

/// The agents the run is about: those named, else a choice among the candidates.
fn pick_agents(
    args: &SetupArgs,
    op: Op,
    env: &Env,
    term: &mut Terminal,
    interactive: bool,
) -> anyhow::Result<Vec<&'static Agent>> {
    if !args.clients.is_empty() {
        let mut named: Vec<_> = args.clients.iter().map(|c| agents::get(*c)).collect();
        named.dedup_by_key(|a| a.client);
        return Ok(named);
    }
    // Removal and status must work for agents that were uninstalled since, so they look at all.
    let candidates: Vec<&Agent> = agents::all()
        .iter()
        .filter(|agent| op != Op::Add || (agent.detect)(env))
        .collect();
    if args.all || op == Op::Status {
        anyhow::ensure!(
            !candidates.is_empty(),
            "no supported agent was found on this machine; name one with --client (claude-code, claude-desktop, cursor, vscode, codex)"
        );
        return Ok(candidates);
    }
    anyhow::ensure!(
        interactive,
        "no agent chosen and no terminal to ask on: pass --client <agent> (repeatable) or --all"
    );
    if candidates.is_empty() {
        anyhow::bail!(
            "no supported agent was found on this machine; name one with --client (claude-code, claude-desktop, cursor, vscode, codex), or use --print to get the entry for one"
        );
    }
    let verb = if op == Op::Add {
        "Register with"
    } else {
        "Remove from"
    };
    let labels: Vec<String> = candidates.iter().map(|a| a.name.to_owned()).collect();
    let chosen = term.choose(verb, &labels)?;
    Ok(chosen.into_iter().map(|i| candidates[i]).collect())
}

/// One plan per agent and scope. Scopes that an agent can't have are skipped, unless the user named
/// the scope for an install, where that is worth an explanation.
fn plans_for(
    agents: &[&'static Agent],
    args: &SetupArgs,
    op: Op,
    wanted: &Wanted,
    env: &Env,
) -> Vec<Plan> {
    let mut plans = Vec::new();
    for agent in agents {
        let scopes = match (op, args.scope) {
            (Op::Add, scope) => vec![scope.unwrap_or(Scope::User)],
            (_, Some(scope)) => vec![scope],
            (Op::Status, None) => agent.status_scopes(env),
            (Op::Remove, None) => [Scope::User, Scope::Project]
                .into_iter()
                .filter(|s| agent.supports(*s, env))
                .collect(),
        };
        let mut seen: Vec<PathBuf> = Vec::new();
        for scope in scopes {
            // Run from the home directory, a project file and the user file are one file.
            let file = agent
                .file(scope, env)
                .map(|f| std::fs::canonicalize(&f).unwrap_or(f));
            if let Some(file) = file {
                if seen.contains(&file) {
                    continue;
                }
                seen.push(file);
            }
            let plan = plan::plan(agent, scope, op, wanted, env);
            // Without a named agent, "not registered here" for every agent is noise.
            let quiet =
                args.clients.is_empty() && op != Op::Status && plan.verdict == Verdict::Absent;
            if !quiet {
                plans.push(plan);
            }
        }
    }
    plans
}

fn report_and_apply(
    plans: Vec<Plan>,
    args: &SetupArgs,
    op: Op,
    wanted: &Wanted,
    env: &Env,
    term: &mut Terminal,
    interactive: bool,
) -> anyhow::Result<()> {
    if plans.is_empty() {
        term.say(&format!(
            "`{}` isn't registered with any agent.",
            wanted.name
        ))?;
        return Ok(());
    }
    if op == Op::Add {
        for scope in scopes_of(&plans) {
            let entry = wanted.registered(scope, env);
            term.say(&format!(
                "Registering `{}` ({} scope) as: {}",
                wanted.name,
                scope.label(),
                entry.summary()
            ))?;
        }
    }
    let changes = plans.iter().any(|p| p.effect != Effect::None);
    for plan in &plans {
        term.say(&line(plan, wanted))?;
    }
    if op == Op::Status || args.dry_run || !changes {
        if args.dry_run && changes {
            term.say("Dry run: nothing was changed.")?;
        }
        return finish(&plans);
    }
    if interactive && !args.yes && !term.confirm("Apply these changes?", true)? {
        term.say("Nothing was changed.")?;
        return Ok(());
    }
    let mut failed = false;
    for plan in &plans {
        if plan.effect == Effect::None {
            continue;
        }
        match apply(&plan.effect) {
            Ok(backup) => {
                term.say(&line_done(plan, wanted))?;
                if let Some(backup) = backup {
                    term.say(&format!("    backup of the old file: {}", backup.display()))?;
                }
            }
            Err(err) => {
                failed = true;
                term.say(&format!("{:<15} FAILED  {err:#}", plan.agent.name))?;
            }
        }
    }
    if !failed && op == Op::Add {
        notes(&plans, wanted, term)?;
    }
    anyhow::ensure!(!failed, "some changes could not be made; see above");
    finish(&plans)
}

fn finish(plans: &[Plan]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !plans.iter().any(|p| p.verdict.is_problem()),
        "some agents were not changed; see above"
    );
    Ok(())
}

fn scopes_of(plans: &[Plan]) -> Vec<Scope> {
    let mut scopes: Vec<Scope> = plans.iter().map(|p| p.scope).collect();
    scopes.dedup();
    scopes
}

/// One planned line: `Cursor          add      ~/.cursor/mcp.json`.
fn line(plan: &Plan, wanted: &Wanted) -> String {
    let name = &wanted.name;
    let scope = if plan.scope == Scope::Project {
        " (project)"
    } else {
        ""
    };
    let what = match &plan.verdict {
        Verdict::Add => format!("add     {}", plan.location),
        Verdict::Replace { existing } => format!("replace {} (was: {existing})", plan.location),
        Verdict::Unchanged => format!("already registered, unchanged: {}", plan.location),
        Verdict::Exists => format!(
            "`{name}` already exists in {}; run `claude mcp get {name}` to compare, or --force to replace it",
            plan.location
        ),
        Verdict::Remove => format!("remove  {}", plan.location),
        Verdict::Absent => "not registered".to_owned(),
        Verdict::Conflict { existing } => format!(
            "SKIPPED a different `{name}` entry is in {} ({existing}); --force replaces it",
            plan.location
        ),
        Verdict::Refused(why) => format!("SKIPPED {why}"),
        Verdict::Present { summary, dangling } => {
            let note = if *dangling {
                "  <- this command no longer exists; run setup again or --remove"
            } else {
                ""
            };
            format!("registered: {summary} ({}){note}", plan.location)
        }
    };
    format!("  {:<15}{scope} {what}", plan.agent.name)
}

fn line_done(plan: &Plan, wanted: &Wanted) -> String {
    let done = match plan.verdict {
        Verdict::Remove => "removed",
        Verdict::Replace { .. } => "replaced",
        _ => "added",
    };
    format!(
        "  {:<15} {done} `{}` in {}",
        plan.agent.name, wanted.name, plan.location
    )
}

/// What to do next, once per agent.
fn notes(plans: &[Plan], wanted: &Wanted, term: &mut Terminal) -> anyhow::Result<()> {
    let mut told: Vec<&str> = Vec::new();
    for plan in plans.iter().filter(|p| p.effect != Effect::None) {
        if !told.contains(&plan.agent.name) {
            told.push(plan.agent.name);
            term.say(&format!("{}: {}", plan.agent.name, plan.agent.after_change))?;
        }
    }
    if wanted.allow_write {
        term.say("Write tools are enabled. Re-run with --force and without --allow-write to turn them off.")?;
    }
    Ok(())
}

/// Carry out one planned change; returns the backup made, if any.
fn apply(effect: &Effect) -> anyhow::Result<Option<PathBuf>> {
    match effect {
        Effect::None => Ok(None),
        Effect::Write { path, contents } => {
            let backup = backup_if_present(path)?;
            files::write_atomically(path, contents)?;
            Ok(backup)
        }
        Effect::Delete { path } => {
            let backup = backup_if_present(path)?;
            std::fs::remove_file(path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            Ok(backup)
        }
        Effect::Run { program, calls } => {
            for CliCall { args, optional } in calls {
                let output = cli::run(program, args, &std::env::current_dir()?)?;
                if !output.status.success() && !optional {
                    anyhow::bail!(
                        "`{} {}` failed: {}",
                        program.display(),
                        args.first().map_or("", String::as_str),
                        cli::failure(&output)
                    );
                }
            }
            Ok(None)
        }
    }
}

fn backup_if_present(path: &std::path::Path) -> anyhow::Result<Option<PathBuf>> {
    if path.exists() {
        files::back_up(path).map(Some)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use clap::Parser;

    use super::{env::Os, *};

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: SetupArgs,
    }

    /// A temporary machine: home, project and `bin` directories, with this "binary" in `bin`.
    struct Machine {
        _dir: tempfile::TempDir,
        env: Env,
    }

    impl Machine {
        fn new(os: Os) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let env = Env::sandbox(dir.path(), os);
            Self { _dir: dir, env }
        }

        fn home(&self, relative: &str) -> PathBuf {
            self.env.home.join(relative)
        }

        /// Run `gitrs-mcp setup <args>`, answering prompts from `answers` (a terminal when given).
        fn run(&self, args: &str, answers: Option<&str>) -> (anyhow::Result<()>, String) {
            let words = args.split_whitespace();
            let parsed = Cli::try_parse_from(std::iter::once("setup").chain(words)).unwrap();
            let mut input = answers.unwrap_or("").as_bytes();
            let mut out = Vec::new();
            let result = execute(
                &parsed.args,
                &self.env,
                &mut Terminal {
                    input: &mut input,
                    out: &mut out,
                },
                answers.is_some(),
            );
            (result, String::from_utf8(out).unwrap())
        }

        fn ok(&self, args: &str) -> String {
            let (result, out) = self.run(args, None);
            result.unwrap_or_else(|err| panic!("{args}: {err:#}\n{out}"));
            out
        }

        /// Every file under the machine, with its bytes.
        fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            fn walk(dir: &Path, into: &mut BTreeMap<PathBuf, Vec<u8>>) {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        walk(&path, into);
                    } else {
                        into.insert(path.clone(), std::fs::read(&path).unwrap());
                    }
                }
            }
            let mut files = BTreeMap::new();
            walk(self._dir.path(), &mut files);
            files
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    const CURSOR: &str = ".cursor/mcp.json";

    #[test]
    fn a_new_file_is_created_and_a_second_run_changes_nothing() {
        let machine = Machine::new(Os::Linux);
        let out = machine.ok("--client cursor --yes");
        assert!(out.contains("added `gitrs`"), "{out}");
        let written = read(&machine.home(CURSOR));
        assert!(written.contains("\"type\": \"stdio\""));
        assert!(written.contains(&machine.env.exe.display().to_string()));

        let before = machine.snapshot();
        let out = machine.ok("--client cursor --yes");
        assert!(out.contains("already registered"), "{out}");
        assert_eq!(machine.snapshot(), before, "no write and no backup");
    }

    #[test]
    fn other_servers_are_kept_and_the_old_file_is_backed_up() {
        let machine = Machine::new(Os::Linux);
        let original =
            "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"x\"\n    }\n  }\n}\n";
        std::fs::create_dir_all(machine.home(".cursor")).unwrap();
        std::fs::write(machine.home(CURSOR), original).unwrap();

        let out = machine.ok("--client cursor --yes");
        assert!(read(&machine.home(CURSOR)).contains("\"other\""));
        let backups: Vec<_> = std::fs::read_dir(machine.home(".cursor"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("gitrs-backup"))
            .collect();
        assert_eq!(backups.len(), 1, "{out}");
        assert_eq!(read(&backups[0]), original);
        assert!(out.contains(&backups[0].display().to_string()));
    }

    #[test]
    fn removing_restores_what_was_there_before() {
        let machine = Machine::new(Os::Linux);
        let created = machine.snapshot();
        machine.ok("--client cursor --yes");
        machine.ok("--client cursor --remove --yes");
        let after: Vec<_> = machine
            .snapshot()
            .into_iter()
            .filter(|(path, _)| !path.to_string_lossy().contains("gitrs-backup"))
            .collect();
        assert_eq!(
            after,
            created.into_iter().collect::<Vec<_>>(),
            "the file setup made is gone"
        );

        let original =
            "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"x\"\n    }\n  }\n}\n";
        std::fs::create_dir_all(machine.home(".cursor")).unwrap();
        std::fs::write(machine.home(CURSOR), original).unwrap();
        machine.ok("--client cursor --yes");
        machine.ok("--client cursor --remove --yes");
        assert_eq!(read(&machine.home(CURSOR)), original);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let machine = Machine::new(Os::Linux);
        std::fs::create_dir_all(machine.home(".cursor")).unwrap();
        std::fs::write(machine.home(CURSOR), "{}").unwrap();
        let before = machine.snapshot();
        let out = machine.ok("--client cursor --client codex --dry-run");
        assert!(
            out.contains("add") && out.contains("Dry run: nothing was changed."),
            "{out}"
        );
        assert_eq!(machine.snapshot(), before);
        machine.ok("--client cursor --remove --dry-run");
        assert_eq!(machine.snapshot(), before);
    }

    #[test]
    fn a_different_entry_needs_force() {
        let machine = Machine::new(Os::Linux);
        std::fs::create_dir_all(machine.home(".codex")).unwrap();
        let theirs = "# mine\n[mcp_servers.gitrs]\ncommand = \"/old/gitrs-mcp\"\n";
        std::fs::write(machine.home(".codex/config.toml"), theirs).unwrap();
        let before = machine.snapshot();

        let (result, out) = machine.run("--client codex --yes", None);
        assert!(result.is_err());
        assert!(
            out.contains("SKIPPED") && out.contains("/old/gitrs-mcp") && out.contains("--force"),
            "{out}"
        );
        assert_eq!(
            machine.snapshot(),
            before,
            "refused, so byte for byte unchanged"
        );

        let out = machine.ok("--client codex --yes --force");
        assert!(out.contains("replaced"), "{out}");
        let now = read(&machine.home(".codex/config.toml"));
        assert!(
            now.starts_with("# mine\n")
                && !now.contains("/old/")
                && now.contains(&machine.env.exe.display().to_string())
        );
    }

    #[test]
    fn files_that_do_not_parse_are_never_overwritten() {
        let machine = Machine::new(Os::Linux);
        std::fs::create_dir_all(machine.home(".vscode")).unwrap();
        std::fs::create_dir_all(machine.env.cwd.join(".vscode")).unwrap();
        let jsonc = "{\n  // my servers\n  \"servers\": {}\n}\n";
        std::fs::write(machine.env.cwd.join(".vscode/mcp.json"), jsonc).unwrap();
        let before = machine.snapshot();
        let (result, out) = machine.run("--client vscode --scope project --yes", None);
        assert!(result.is_err());
        assert!(
            out.contains("left alone") && out.contains("comments"),
            "{out}"
        );
        assert_eq!(machine.snapshot(), before);
    }

    #[test]
    fn project_scope_uses_the_bare_command_and_the_project_file() {
        let machine = Machine::new(Os::Linux);
        machine.ok("--client cursor --client vscode --client codex --scope project --yes --repo . --allow-write --watch");
        let cursor = read(&machine.env.cwd.join(CURSOR));
        assert!(cursor.contains("\"command\": \"gitrs-mcp\""), "{cursor}");
        for flag in ["--repo", "\".\"", "--allow-write", "--watch"] {
            assert!(cursor.contains(flag), "{flag} in {cursor}");
        }
        assert!(read(&machine.env.cwd.join(".vscode/mcp.json")).contains("\"servers\""));
        assert!(read(&machine.env.cwd.join(".codex/config.toml")).contains("[mcp_servers.gitrs]"));
        assert!(!machine.home(CURSOR).exists(), "nothing at user level");
    }

    #[test]
    fn run_from_the_home_directory_the_project_file_is_the_user_file() {
        let mut machine = Machine::new(Os::Linux);
        machine.env.cwd = machine.env.home.clone();
        machine.ok("--client codex --yes");
        let out = machine.ok("--client codex --remove --yes");
        assert_eq!(out.matches("removed `gitrs`").count(), 1, "{out}");
        assert!(!machine.home(".codex/config.toml").exists());
    }

    #[test]
    fn user_scope_repositories_are_absolute() {
        let machine = Machine::new(Os::Linux);
        machine.ok("--client cursor --yes --repo ../elsewhere");
        let written = read(&machine.home(CURSOR));
        let expected = std::path::absolute(machine.env.cwd.join("../elsewhere")).unwrap();
        assert!(
            written.contains(&expected.display().to_string()),
            "{written}"
        );
    }

    #[test]
    fn unsupported_scopes_are_explained() {
        let machine = Machine::new(Os::Linux);
        let (result, out) = machine.run("--client claude-desktop --yes", None);
        assert!(
            result.is_err() && out.contains("no user configuration"),
            "{out}"
        );
        let (result, out) = machine.run("--client claude-desktop --scope project --yes", None);
        assert!(
            result.is_err() && out.contains("no project configuration"),
            "{out}"
        );
        let mac = Machine::new(Os::Mac);
        mac.ok("--client claude-desktop --yes");
        assert!(
            read(&mac.home("Library/Application Support/Claude/claude_desktop_config.json"))
                .contains("mcpServers")
        );
    }

    #[test]
    fn without_a_terminal_setup_never_asks() {
        let machine = Machine::new(Os::Linux);
        let (result, _) = machine.run("", None);
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("--client") && message.contains("--all"),
            "{message}"
        );
        let (result, _) = machine.run("--all", None);
        assert!(format!("{:#}", result.unwrap_err()).contains("no supported agent"));
    }

    #[test]
    fn all_registers_the_detected_agents_only() {
        let machine = Machine::new(Os::Linux);
        std::fs::create_dir_all(machine.home(".codex")).unwrap();
        machine.ok("--all --yes");
        assert!(machine.home(".codex/config.toml").exists());
        assert!(!machine.home(CURSOR).exists());
    }

    #[test]
    fn a_terminal_gets_a_checklist_a_write_question_and_a_confirmation() {
        let machine = Machine::new(Os::Linux);
        std::fs::create_dir_all(machine.home(".cursor")).unwrap();
        std::fs::create_dir_all(machine.home(".codex")).unwrap();

        // Declining the confirmation changes nothing.
        let before = machine.snapshot();
        let (result, out) = machine.run("", Some("all\nn\nn\n"));
        result.unwrap();
        assert!(
            out.contains("1) Cursor")
                && out.contains("2) Codex CLI")
                && out.contains("Nothing was changed."),
            "{out}"
        );
        assert_eq!(machine.snapshot(), before);

        // Choosing Codex only, enabling write tools, confirming.
        let (result, out) = machine.run("", Some("2\ny\ny\n"));
        result.unwrap();
        assert!(!machine.home(CURSOR).exists(), "{out}");
        assert!(read(&machine.home(".codex/config.toml")).contains("--allow-write"));
        assert!(out.contains("Write tools are enabled"), "{out}");
    }

    #[test]
    fn write_tools_stay_off_unless_asked_for() {
        let machine = Machine::new(Os::Linux);
        machine.ok("--client codex --yes");
        assert!(!read(&machine.home(".codex/config.toml")).contains("--allow-write"));
    }

    #[test]
    fn status_reports_registrations_and_dangling_commands() {
        let machine = Machine::new(Os::Linux);
        std::fs::create_dir_all(machine.home(".cursor")).unwrap();
        std::fs::write(
            machine.home(CURSOR),
            "{\"mcpServers\": {\"gitrs\": {\"command\": \"/gone/gitrs-mcp\"}}}",
        )
        .unwrap();
        let out = machine.ok("--status");
        assert!(
            out.contains("registered: /gone/gitrs-mcp") && out.contains("no longer exists"),
            "{out}"
        );
        assert!(
            out.contains("Codex CLI") && out.contains("not registered"),
            "{out}"
        );

        std::fs::write(
            machine.home(CURSOR),
            "{\"mcpServers\": {\"gitrs\": {\"command\": \"gitrs-mcp\"}}}",
        )
        .unwrap();
        assert!(
            machine
                .ok("--status --client cursor --scope user")
                .contains("no longer exists"),
            "not on PATH"
        );
    }

    #[test]
    fn removing_without_a_named_agent_reports_what_it_found() {
        let machine = Machine::new(Os::Linux);
        let out = machine.ok("--remove --all --yes");
        assert!(out.contains("isn't registered with any agent"), "{out}");
        machine.ok("--client codex --yes");
        let out = machine.ok("--remove --all --yes");
        assert!(
            out.contains("removed `gitrs`") && !out.contains("not registered"),
            "{out}"
        );
    }

    #[test]
    fn print_shows_the_entry_for_one_agent_and_changes_nothing() {
        let machine = Machine::new(Os::Linux);
        let before = machine.snapshot();
        let out = machine.ok("--print --client codex --repo /work");
        assert!(
            out.contains("[mcp_servers.gitrs]") && out.contains("\"/work\""),
            "{out}"
        );
        assert!(out.contains("config.toml") && out.contains("format checked"));
        let out = machine.ok("--print --client claude-code --scope project");
        assert!(
            out.starts_with("Run this")
                && out.contains("claude mcp add-json gitrs '{")
                && out.contains("--scope project"),
            "{out}"
        );
        assert!(
            machine
                .ok("--print --client vscode")
                .contains("\"servers\"")
        );
        assert_eq!(machine.snapshot(), before);

        assert!(machine.run("--print", None).0.is_err(), "needs a client");
        assert!(
            machine
                .run("--print --client claude-desktop", None)
                .0
                .is_err(),
            "no Linux path"
        );
    }

    #[cfg(unix)]
    mod claude {
        use super::*;

        /// A `claude` that records its arguments and pretends the server is registered when the
        /// file `registered` exists.
        fn fake_claude(machine: &Machine) -> PathBuf {
            let log = machine._dir.path().join("claude.log");
            machine.env.install_program(
                "claude",
                &format!(
                    "echo \"$@\" >> {log}\ncase \"$1 $2\" in\n  \"mcp get\") test -f {registered};;\n  *) exit 0;;\nesac",
                    log = log.display(),
                    registered = machine._dir.path().join("registered").display(),
                ),
            );
            log
        }

        fn calls(log: &Path) -> Vec<String> {
            read(log).lines().map(str::to_owned).collect()
        }

        #[test]
        fn claude_code_is_driven_through_its_own_cli() {
            let machine = Machine::new(Os::Linux);
            let log = fake_claude(&machine);
            machine.ok("--client claude-code --yes --repo /work");
            let calls = calls(&log);
            assert_eq!(calls[0], "mcp get gitrs");
            let command = format!(
                "{{\"type\":\"stdio\",\"command\":\"{}\",\"args\":[\"--repo\",\"/work\"]}}",
                machine.env.exe.display()
            );
            assert_eq!(
                calls[1],
                format!("mcp add-json gitrs {command} --scope user")
            );
            assert_eq!(calls.len(), 2);
        }

        #[test]
        fn an_existing_claude_entry_needs_force_and_is_replaced_with_it() {
            let machine = Machine::new(Os::Linux);
            let log = fake_claude(&machine);
            std::fs::write(machine._dir.path().join("registered"), "").unwrap();

            let out = machine.ok("--client claude-code --yes --scope project");
            assert!(
                out.contains("already exists") && out.contains("--force"),
                "{out}"
            );
            assert_eq!(calls(&log).len(), 1, "only the lookup");

            machine.ok("--client claude-code --yes --scope project --force");
            let calls = calls(&log);
            assert_eq!(calls[2], "mcp remove gitrs --scope project");
            assert!(
                calls[3].starts_with(
                    "mcp add-json gitrs {\"type\":\"stdio\",\"command\":\"gitrs-mcp\""
                ),
                "bare command at project scope: {calls:?}"
            );
        }

        #[test]
        fn removing_and_status_use_the_cli() {
            let machine = Machine::new(Os::Linux);
            let log = fake_claude(&machine);
            assert!(
                machine
                    .ok("--client claude-code --remove --yes")
                    .contains("not registered")
            );
            std::fs::write(machine._dir.path().join("registered"), "").unwrap();
            assert!(
                machine
                    .ok("--status --client claude-code")
                    .contains("claude mcp get gitrs")
            );
            machine.ok("--client claude-code --remove --yes");
            let calls = calls(&log);
            assert!(
                calls.contains(&"mcp remove gitrs --scope user".to_owned()),
                "{calls:?}"
            );
            assert!(
                calls.contains(&"mcp remove gitrs --scope project".to_owned()),
                "{calls:?}"
            );
        }

        #[test]
        fn a_failing_claude_is_reported_and_the_run_fails() {
            let machine = Machine::new(Os::Linux);
            machine.env.install_program(
                "claude",
                "case \"$2\" in get) exit 1;; *) echo 'boom' >&2; exit 2;; esac",
            );
            let (result, out) = machine.run("--client claude-code --yes", None);
            assert!(
                result.is_err() && out.contains("FAILED") && out.contains("boom"),
                "{out}"
            );
        }

        #[test]
        fn claude_missing_from_path_is_explained() {
            let machine = Machine::new(Os::Linux);
            let (result, out) = machine.run("--client claude-code --yes", None);
            assert!(
                result.is_err()
                    && out.contains("`claude` isn't on PATH")
                    && out.contains("--print"),
                "{out}"
            );
        }
    }
}
