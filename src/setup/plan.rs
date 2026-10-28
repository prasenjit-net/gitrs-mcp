//! Working out what setup would do for one agent and scope, without doing it.
//!
//! Planning only reads: config files, and `claude mcp get`. [`Plan::effect`] says what applying
//! it would change, so `--dry-run` and the confirmation can show it first.

use std::path::{Path, PathBuf};

use super::{
    Scope,
    agents::{Agent, Format},
    cli,
    document::{Document, Existing, Registered},
    env::Env,
    files,
    json::JsonDocument,
    toml::TomlDocument,
};

/// What the user asked to have registered.
pub(crate) struct Wanted {
    /// The key the entry goes under.
    pub name: String,
    pub repos: Vec<PathBuf>,
    pub allow_write: bool,
    pub watch: bool,
    /// Replace an existing, different entry.
    pub force: bool,
}

impl Wanted {
    /// The command and arguments for `scope`. Repository paths in user-level entries are made
    /// absolute, since the client starts the server somewhere else; project files keep them as
    /// written so they stay portable.
    pub fn registered(&self, scope: Scope, env: &Env) -> Registered {
        let mut args = Vec::new();
        for repo in &self.repos {
            let repo = match scope {
                Scope::User => {
                    std::path::absolute(env.cwd.join(repo)).unwrap_or_else(|_| repo.clone())
                }
                Scope::Project => repo.clone(),
            };
            args.push("--repo".to_owned());
            args.push(repo.display().to_string());
        }
        if self.allow_write {
            args.push("--allow-write".to_owned());
        }
        if self.watch {
            args.push("--watch".to_owned());
        }
        Registered {
            command: super::binary::command_for(scope, &env.exe),
            args,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Add,
    Remove,
    Status,
}

/// What would happen, or for `--status` what is there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Add,
    Replace {
        existing: String,
    },
    /// Already registered exactly as asked.
    Unchanged,
    /// An entry exists that setup can't compare (the CLI-driven agent).
    Exists,
    Remove,
    Absent,
    /// A different entry is there and `--force` wasn't given.
    Conflict {
        existing: String,
    },
    /// Setup won't touch it, and says why.
    Refused(String),
    /// `--status`: registered, with the entry as written. `dangling` when its command is missing.
    Present {
        summary: String,
        dangling: bool,
    },
}

impl Verdict {
    /// Whether this should make the run fail.
    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Conflict { .. } | Self::Refused(_))
    }
}

/// A command line of the agent's own CLI.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CliCall {
    pub args: Vec<String>,
    /// A failure doesn't stop the run (removing an entry that lives in another scope).
    pub optional: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    None,
    Write {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
    },
    Run {
        program: PathBuf,
        calls: Vec<CliCall>,
    },
}

pub(crate) struct Plan {
    pub agent: &'static Agent,
    pub scope: Scope,
    /// The file, or the command, that holds the registration.
    pub location: String,
    pub verdict: Verdict,
    pub effect: Effect,
}

pub(crate) fn plan(
    agent: &'static Agent,
    scope: Scope,
    op: Op,
    wanted: &Wanted,
    env: &Env,
) -> Plan {
    let server = wanted.registered(scope, env);
    let (location, verdict, effect) = if !agent.supports(scope, env) {
        let why = format!(
            "{} has no {} configuration setup can edit",
            agent.name,
            scope.label()
        );
        (agent.name.to_owned(), Verdict::Refused(why), Effect::None)
    } else if let Format::ClaudeCli = agent.format {
        let (verdict, effect) = plan_claude(scope, op, wanted, &server, env);
        (
            format!("claude mcp, {} scope", scope.label()),
            verdict,
            effect,
        )
    } else {
        let path = agent.file(scope, env).unwrap_or_default();
        let (verdict, effect) = plan_file(agent, &path, op, wanted, &server, env);
        (path.display().to_string(), verdict, effect)
    };
    Plan {
        agent,
        scope,
        location,
        verdict,
        effect,
    }
}

fn plan_file(
    agent: &Agent,
    path: &Path,
    op: Op,
    wanted: &Wanted,
    server: &Registered,
    env: &Env,
) -> (Verdict, Effect) {
    let refused = |why: String| {
        (
            Verdict::Refused(format!("{} was left alone: {why}", path.display())),
            Effect::None,
        )
    };
    let text = match files::read_optional(path) {
        Ok(text) => text.unwrap_or_default(),
        Err(err) => return refused(format!("{err:#}")),
    };
    match agent.format {
        Format::Json { servers_key, typed } => match JsonDocument::parse(&text, servers_key, typed)
        {
            Ok(mut doc) => edit(&mut doc, path, op, wanted, server, env),
            Err(why) => refused(why),
        },
        Format::Toml => match TomlDocument::parse(&text) {
            Ok(mut doc) => edit(&mut doc, path, op, wanted, server, env),
            Err(why) => refused(why),
        },
        Format::ClaudeCli => (Verdict::Absent, Effect::None),
    }
}

/// The add, remove or status logic, shared by every file format.
fn edit(
    doc: &mut impl Document,
    path: &Path,
    op: Op,
    wanted: &Wanted,
    server: &Registered,
    env: &Env,
) -> (Verdict, Effect) {
    let name = wanted.name.as_str();
    let write = |doc: &dyn Document| Effect::Write {
        path: path.to_path_buf(),
        contents: doc.render(),
    };
    match op {
        Op::Status => (status(doc.get(name), env), Effect::None),
        Op::Add => match doc.get(name) {
            Existing::Missing => {
                doc.set(name, server);
                (Verdict::Add, write(doc))
            }
            Existing::Entry(found) if found == *server => (Verdict::Unchanged, Effect::None),
            found => {
                let existing = describe(&found);
                if !wanted.force {
                    return (Verdict::Conflict { existing }, Effect::None);
                }
                doc.set(name, server);
                (Verdict::Replace { existing }, write(doc))
            }
        },
        Op::Remove => {
            if !doc.remove(name) {
                return (Verdict::Absent, Effect::None);
            }
            let effect = if doc.is_empty() {
                Effect::Delete {
                    path: path.to_path_buf(),
                }
            } else {
                write(doc)
            };
            (Verdict::Remove, effect)
        }
    }
}

fn describe(existing: &Existing) -> String {
    match existing {
        Existing::Missing => String::new(),
        Existing::Entry(entry) => entry.summary(),
        Existing::Other(raw) => raw.clone(),
    }
}

fn status(existing: Existing, env: &Env) -> Verdict {
    let dangling = match &existing {
        Existing::Entry(entry) => !command_exists(&entry.command, env),
        _ => false,
    };
    match existing {
        Existing::Missing => Verdict::Absent,
        found => Verdict::Present {
            summary: describe(&found),
            dangling,
        },
    }
}

/// Whether a registered command would start: an existing file for a path, a `PATH` hit for a name.
fn command_exists(command: &str, env: &Env) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        path.is_file()
    } else {
        env.which(command).is_some()
    }
}

fn plan_claude(
    scope: Scope,
    op: Op,
    wanted: &Wanted,
    server: &Registered,
    env: &Env,
) -> (Verdict, Effect) {
    let Some(program) = env.which("claude") else {
        // Without the CLI nothing can be registered through it, and there is nothing to undo.
        if op != Op::Add {
            return (Verdict::Absent, Effect::None);
        }
        return (
            Verdict::Refused("`claude` isn't on PATH; install Claude Code, or use `--print` for the command to run by hand".into()),
            Effect::None,
        );
    };
    let name = wanted.name.as_str();
    let get = vec!["mcp".to_owned(), "get".to_owned(), name.to_owned()];
    let registered = match cli::run(&program, &get, &env.cwd) {
        Ok(output) => output.status.success(),
        Err(err) => return (Verdict::Refused(format!("{err:#}")), Effect::None),
    };
    let scope_arg = scope.label().to_owned();
    let remove = || CliCall {
        args: ["mcp", "remove", name, "--scope", &scope_arg]
            .map(str::to_owned)
            .to_vec(),
        optional: false,
    };
    let add = || CliCall {
        args: [
            "mcp",
            "add-json",
            name,
            &add_json(server),
            "--scope",
            &scope_arg,
        ]
        .map(str::to_owned)
        .to_vec(),
        optional: false,
    };
    let run = |calls| Effect::Run {
        program: program.clone(),
        calls,
    };
    match (op, registered) {
        (Op::Status, true) => (
            Verdict::Present {
                summary: format!("see `claude mcp get {name}`"),
                dangling: false,
            },
            Effect::None,
        ),
        (Op::Add, false) => (Verdict::Add, run(vec![add()])),
        (Op::Add, true) if wanted.force => (
            Verdict::Replace {
                existing: format!("see `claude mcp get {name}`"),
            },
            run(vec![
                CliCall {
                    optional: true,
                    ..remove()
                },
                add(),
            ]),
        ),
        (Op::Add, true) => (Verdict::Exists, Effect::None),
        (Op::Remove, true) => (Verdict::Remove, run(vec![remove()])),
        (_, false) => (Verdict::Absent, Effect::None),
    }
}

/// The JSON `claude mcp add-json` takes.
fn add_json(server: &Registered) -> String {
    serde_json::json!({"type": "stdio", "command": server.command, "args": server.args}).to_string()
}

/// The registration written out for `--print`: the file text, or the command to run.
pub(crate) fn snippet(agent: &Agent, scope: Scope, wanted: &Wanted, env: &Env) -> String {
    let server = wanted.registered(scope, env);
    let name = wanted.name.as_str();
    match agent.format {
        Format::Json { servers_key, typed } => {
            let mut doc = JsonDocument::empty(servers_key, typed);
            doc.set(name, &server);
            doc.render()
        }
        Format::Toml => {
            let mut doc = TomlDocument::empty();
            doc.set(name, &server);
            doc.render()
        }
        Format::ClaudeCli => format!(
            "claude mcp add-json {name} {} --scope {}\n",
            shell_quote(&add_json(&server)),
            scope.label()
        ),
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}
