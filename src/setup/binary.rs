//! Which command to register for the server.

use std::path::{Component, Path, PathBuf};

use super::Scope;

/// The command name that project files use, so a committed file works on every machine.
const BARE_COMMAND: &str = "gitrs-mcp";

/// The command to register.
///
/// A project file gets the bare name, found on each teammate's `PATH`. User-level entries get an
/// absolute path, since apps launched from the Dock or Start menu don't see the shell's `PATH`.
pub(crate) fn command_for(scope: Scope, exe: &Path) -> String {
    match scope {
        Scope::Project => BARE_COMMAND.to_owned(),
        Scope::User => stable_path(exe).display().to_string(),
    }
}

/// `exe`, unless it is inside a Homebrew `Cellar/<name>/<version>/`, which `brew upgrade` deletes:
/// then the `bin` link in the same prefix, which it keeps pointing at the current version.
fn stable_path(exe: &Path) -> PathBuf {
    let components: Vec<Component> = exe.components().collect();
    let Some(cellar) = components.iter().position(|c| c.as_os_str() == "Cellar") else {
        return exe.to_path_buf();
    };
    let prefix: PathBuf = components[..cellar].iter().collect();
    let link = prefix.join("bin").join(exe.file_name().unwrap_or_default());
    if link.exists() {
        link
    } else {
        exe.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_files_get_the_bare_name() {
        assert_eq!(
            command_for(Scope::Project, Path::new("/opt/x/gitrs-mcp")),
            "gitrs-mcp"
        );
    }

    #[test]
    fn user_entries_get_the_absolute_path() {
        assert_eq!(
            command_for(Scope::User, Path::new("/home/me/.local/bin/gitrs-mcp")),
            "/home/me/.local/bin/gitrs-mcp"
        );
    }

    #[test]
    fn homebrew_cellar_paths_become_the_bin_link() {
        let prefix = tempfile::tempdir().unwrap();
        let cellar_exe = prefix.path().join("Cellar/gitrs-mcp/0.1.0/bin/gitrs-mcp");
        assert_eq!(
            command_for(Scope::User, &cellar_exe),
            cellar_exe.display().to_string(),
            "without a bin link there is nothing better"
        );
        std::fs::create_dir_all(prefix.path().join("bin")).unwrap();
        std::fs::write(prefix.path().join("bin/gitrs-mcp"), "").unwrap();
        assert_eq!(
            command_for(Scope::User, &cellar_exe),
            prefix.path().join("bin/gitrs-mcp").display().to_string()
        );
    }
}
