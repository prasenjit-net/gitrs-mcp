//! Everything setup reads from the machine, gathered once so tests can substitute a temporary home.

use std::path::{Path, PathBuf};

use anyhow::Context;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Os {
    Mac,
    Linux,
    Windows,
}

impl Os {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// The locations setup looks at. Nothing outside this struct consults the environment.
#[derive(Clone, Debug)]
pub(crate) struct Env {
    pub os: Os,
    pub home: PathBuf,
    /// `%APPDATA%`, set on Windows only.
    pub appdata: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`, when set.
    pub xdg_config: Option<PathBuf>,
    /// The directories of `PATH`, in order.
    pub path: Vec<PathBuf>,
    /// The directory setup was run in: the project for `--scope project`.
    pub cwd: PathBuf,
    /// This executable, which is what gets registered.
    pub exe: PathBuf,
}

impl Env {
    pub fn from_process() -> anyhow::Result<Self> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME")
            .or_else(|| var("USERPROFILE"))
            .context("cannot find your home directory: neither HOME nor USERPROFILE is set")?;
        Ok(Self {
            os: Os::current(),
            home,
            appdata: var("APPDATA"),
            xdg_config: var("XDG_CONFIG_HOME"),
            path: std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
            cwd: std::env::current_dir().context("cannot read the working directory")?,
            exe: std::env::current_exe().context("cannot find this executable's path")?,
        })
    }

    /// `$XDG_CONFIG_HOME`, or `~/.config`.
    pub fn config_home(&self) -> PathBuf {
        self.xdg_config
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
    }

    /// The program a shell would run for `name`, found on `PATH`.
    pub fn which(&self, name: &str) -> Option<PathBuf> {
        let suffixes: &[&str] = if self.os == Os::Windows {
            &["", ".exe", ".cmd", ".bat"]
        } else {
            &[""]
        };
        self.path
            .iter()
            .flat_map(|dir| {
                suffixes
                    .iter()
                    .map(move |suffix| dir.join(format!("{name}{suffix}")))
            })
            .find(|candidate| is_executable(candidate))
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
impl Env {
    /// A machine whose home, project and `bin` directory are inside `root`, for tests.
    pub fn sandbox(root: &Path, os: Os) -> Self {
        let home = root.join("home");
        let bin = root.join("bin");
        let project = root.join("project");
        for dir in [&home, &bin, &project] {
            std::fs::create_dir_all(dir).expect("sandbox directory");
        }
        Self {
            os,
            appdata: (os == Os::Windows).then(|| home.join("AppData")),
            xdg_config: None,
            path: vec![bin.clone()],
            cwd: project,
            exe: bin.join("gitrs-mcp"),
            home,
        }
    }

    /// Put an executable shell script named `name` on the sandbox `PATH`.
    #[cfg(unix)]
    pub fn install_program(&self, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.path[0].join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("program written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn which_finds_executables_on_path_only() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::sandbox(dir.path(), Os::Linux);
        assert_eq!(env.which("claude"), None);
        let program = env.install_program("claude", "exit 0");
        assert_eq!(env.which("claude"), Some(program));
        std::fs::write(env.path[0].join("plain"), "not executable").unwrap();
        assert_eq!(env.which("plain"), None, "a file without the execute bit");
    }

    #[test]
    fn config_home_follows_xdg() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Env::sandbox(dir.path(), Os::Linux);
        assert_eq!(env.config_home(), env.home.join(".config"));
        env.xdg_config = Some(dir.path().join("xdg"));
        assert_eq!(env.config_home(), dir.path().join("xdg"));
    }
}
