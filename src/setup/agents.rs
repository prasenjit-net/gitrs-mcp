//! The registry: one entry per agent. Adding an agent means adding an entry here, its tests, and a
//! line in the docs; the planning and applying code doesn't change.

use std::path::PathBuf;

use super::{
    Scope,
    env::{Env, Os},
};

/// The agents setup knows, as `--client` spells them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Vscode,
    Codex,
}

/// How an agent's registration is read and written.
pub(crate) enum Format {
    /// A JSON file with the servers under `servers_key`; `typed` adds `"type": "stdio"` to entries.
    Json {
        servers_key: &'static str,
        typed: bool,
    },
    /// A TOML file with `[mcp_servers.<name>]` tables.
    Toml,
    /// The `claude mcp` command, which owns the file and its scope rules.
    ClaudeCli,
}

pub(crate) struct Agent {
    pub client: Client,
    pub name: &'static str,
    pub format: Format,
    /// Whether the agent seems to be installed on this machine.
    pub detect: fn(&Env) -> bool,
    /// The user-level config file, where the agent has one we edit and this OS is supported.
    pub user_file: fn(&Env) -> Option<PathBuf>,
    /// The project-level config file, relative to the project directory.
    pub project_file: Option<&'static str>,
    /// What to tell the user after a change.
    pub after_change: &'static str,
    /// When the format and paths were last checked against the agent's documentation.
    /// Printed by `--print`, so a snippet says how fresh it is. The manual release matrix updates it.
    pub verified: &'static str,
}

impl Agent {
    pub fn supports(&self, scope: Scope, env: &Env) -> bool {
        match (&self.format, scope) {
            (Format::ClaudeCli, _) => true,
            (_, Scope::User) => (self.user_file)(env).is_some(),
            (_, Scope::Project) => self.project_file.is_some(),
        }
    }

    /// The file edited for `scope`; `None` for the CLI-driven agent.
    pub fn file(&self, scope: Scope, env: &Env) -> Option<PathBuf> {
        match scope {
            Scope::User => (self.user_file)(env),
            Scope::Project => self.project_file.map(|file| env.cwd.join(file)),
        }
    }

    /// The scopes worth reporting in `--status`: every one for file-based agents, but the CLI
    /// reports all of its scopes at once.
    pub fn status_scopes(&self, env: &Env) -> Vec<Scope> {
        let scopes: &[Scope] = match self.format {
            Format::ClaudeCli => &[Scope::User],
            _ => &[Scope::User, Scope::Project],
        };
        scopes
            .iter()
            .copied()
            .filter(|s| self.supports(*s, env))
            .collect()
    }
}

pub(crate) fn all() -> &'static [Agent] {
    &AGENTS
}

pub(crate) fn get(client: Client) -> &'static Agent {
    AGENTS
        .iter()
        .find(|agent| agent.client == client)
        .unwrap_or(&AGENTS[0])
}

static AGENTS: [Agent; 5] = [
    Agent {
        client: Client::ClaudeCode,
        name: "Claude Code",
        format: Format::ClaudeCli,
        detect: |env| env.which("claude").is_some(),
        user_file: |_| None,
        project_file: None,
        after_change: "Start a new Claude Code session; `/mcp` lists the server. Project-scoped servers ask for approval the first time.",
        verified: "2026-10",
    },
    Agent {
        client: Client::ClaudeDesktop,
        name: "Claude Desktop",
        format: Format::Json {
            servers_key: "mcpServers",
            typed: false,
        },
        detect: |env| {
            claude_desktop_file(env).is_some_and(|f| f.parent().is_some_and(|d| d.is_dir()))
        },
        user_file: claude_desktop_file,
        project_file: None,
        after_change: "Quit Claude Desktop completely and reopen it. It starts servers outside any project, so pass --repo <path> to give this one a repository.",
        verified: "2026-10",
    },
    Agent {
        client: Client::Cursor,
        name: "Cursor",
        format: Format::Json {
            servers_key: "mcpServers",
            typed: true,
        },
        detect: |env| env.which("cursor").is_some() || env.home.join(".cursor").is_dir(),
        user_file: |env| Some(env.home.join(".cursor/mcp.json")),
        project_file: Some(".cursor/mcp.json"),
        after_change: "Reload Cursor; the server shows under Settings > MCP.",
        verified: "2026-10",
    },
    Agent {
        client: Client::Vscode,
        name: "VS Code",
        format: Format::Json {
            servers_key: "servers",
            typed: true,
        },
        detect: |env| {
            env.which("code").is_some()
                || vscode_file(env).is_some_and(|f| f.parent().is_some_and(|d| d.is_dir()))
        },
        user_file: vscode_file,
        project_file: Some(".vscode/mcp.json"),
        after_change: "Run \"MCP: List Servers\" in VS Code to start it and confirm trust.",
        verified: "2026-10",
    },
    Agent {
        client: Client::Codex,
        name: "Codex CLI",
        format: Format::Toml,
        detect: |env| env.which("codex").is_some() || env.home.join(".codex").is_dir(),
        user_file: |env| Some(env.home.join(".codex/config.toml")),
        project_file: Some(".codex/config.toml"),
        after_change: "Start a new Codex session. A project's .codex/config.toml only applies once you trust the project.",
        verified: "2026-10",
    },
];

fn claude_desktop_file(env: &Env) -> Option<PathBuf> {
    let file = "claude_desktop_config.json";
    match env.os {
        Os::Mac => Some(
            env.home
                .join("Library/Application Support/Claude")
                .join(file),
        ),
        Os::Windows => env
            .appdata
            .as_ref()
            .map(|dir| dir.join("Claude").join(file)),
        // Anthropic documents no Linux path for the desktop app.
        Os::Linux => None,
    }
}

fn vscode_file(env: &Env) -> Option<PathBuf> {
    let user = match env.os {
        Os::Mac => env.home.join("Library/Application Support/Code/User"),
        Os::Windows => env.appdata.as_ref()?.join("Code/User"),
        Os::Linux => env.config_home().join("Code/User"),
    };
    Some(user.join("mcp.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(os: Os) -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::sandbox(dir.path(), os);
        (dir, env)
    }

    #[test]
    fn every_client_has_exactly_one_entry() {
        use clap::ValueEnum;
        for client in Client::value_variants() {
            assert_eq!(all().iter().filter(|a| a.client == *client).count(), 1);
            assert_eq!(get(*client).client, *client);
        }
    }

    #[test]
    fn paths_follow_each_operating_system() {
        let (_d, mac) = sandbox(Os::Mac);
        assert!(
            claude_desktop_file(&mac)
                .unwrap()
                .ends_with("Library/Application Support/Claude/claude_desktop_config.json")
        );
        assert!(
            vscode_file(&mac)
                .unwrap()
                .ends_with("Library/Application Support/Code/User/mcp.json")
        );
        let (_d, linux) = sandbox(Os::Linux);
        assert_eq!(claude_desktop_file(&linux), None);
        assert!(
            vscode_file(&linux)
                .unwrap()
                .ends_with(".config/Code/User/mcp.json")
        );
        let (_d, windows) = sandbox(Os::Windows);
        assert!(
            claude_desktop_file(&windows)
                .unwrap()
                .ends_with("AppData/Claude/claude_desktop_config.json")
        );
        assert!(
            vscode_file(&windows)
                .unwrap()
                .ends_with("AppData/Code/User/mcp.json")
        );
    }

    #[test]
    fn scopes_are_supported_where_a_file_exists() {
        let (_d, linux) = sandbox(Os::Linux);
        let desktop = get(Client::ClaudeDesktop);
        assert!(
            !desktop.supports(Scope::User, &linux) && !desktop.supports(Scope::Project, &linux)
        );
        let cursor = get(Client::Cursor);
        assert!(cursor.supports(Scope::User, &linux) && cursor.supports(Scope::Project, &linux));
        assert_eq!(
            cursor.file(Scope::Project, &linux).unwrap(),
            linux.cwd.join(".cursor/mcp.json")
        );
        assert!(get(Client::ClaudeCode).supports(Scope::Project, &linux));
        assert_eq!(get(Client::ClaudeCode).status_scopes(&linux), [Scope::User]);
        assert_eq!(cursor.status_scopes(&linux).len(), 2);
    }

    #[test]
    fn agents_are_detected_by_their_directories() {
        let (_d, env) = sandbox(Os::Mac);
        let detected = |env: &Env| {
            all()
                .iter()
                .filter(|a| (a.detect)(env))
                .map(|a| a.client)
                .collect::<Vec<_>>()
        };
        assert_eq!(detected(&env), []);
        std::fs::create_dir_all(env.home.join(".cursor")).unwrap();
        std::fs::create_dir_all(env.home.join(".codex")).unwrap();
        std::fs::create_dir_all(env.home.join("Library/Application Support/Claude")).unwrap();
        std::fs::create_dir_all(env.home.join("Library/Application Support/Code/User")).unwrap();
        assert_eq!(
            detected(&env),
            [
                Client::ClaudeDesktop,
                Client::Cursor,
                Client::Vscode,
                Client::Codex
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn agents_are_detected_by_their_programs() {
        let (_d, env) = sandbox(Os::Linux);
        env.install_program("claude", "exit 0");
        assert!((get(Client::ClaudeCode).detect)(&env));
        env.install_program("code", "exit 0");
        assert!((get(Client::Vscode).detect)(&env));
    }
}
