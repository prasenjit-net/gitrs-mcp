//! Changing files that belong to other applications: back up first, then replace atomically.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;

/// The file's text, or `None` when it doesn't exist.
pub(crate) fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Copy `path` to `<path>.gitrs-backup-<unix seconds>` and return the copy's path. An earlier backup
/// from the same second is never overwritten; the new one gets a numeric suffix.
pub(crate) fn back_up(path: &Path) -> anyhow::Result<PathBuf> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let backup = (1..)
        .map(|n| match n {
            1 => path.with_file_name(format!("{file_name}.gitrs-backup-{seconds}")),
            n => path.with_file_name(format!("{file_name}.gitrs-backup-{seconds}-{n}")),
        })
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| path.with_extension("gitrs-backup"));
    std::fs::copy(path, &backup)
        .with_context(|| format!("cannot back up {} to {}", path.display(), backup.display()))?;
    Ok(backup)
}

/// Replace `path` with `contents` through a temporary file in the same directory, keeping the
/// existing file's permissions: these files can hold other servers' secrets. Creates missing
/// parent directories.
pub(crate) fn write_atomically(path: &Path, contents: &str) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let temp = dir.join(format!(".gitrs-setup-{}.tmp", std::process::id()));
    let result = write_then_rename(&temp, path, contents);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn write_then_rename(temp: &Path, path: &Path, contents: &str) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .with_context(|| format!("cannot write in {}", temp.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    if let Ok(existing) = std::fs::metadata(path) {
        file.set_permissions(existing.permissions())
            .with_context(|| format!("cannot copy the permissions of {}", path.display()))?;
    }
    drop(file);
    std::fs::rename(temp, path).with_context(|| format!("cannot replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_optional(&dir.path().join("nope")).unwrap(), None);
        std::fs::write(dir.path().join("a"), "x").unwrap();
        assert_eq!(read_optional(&dir.path().join("a")).unwrap().unwrap(), "x");
        assert!(
            read_optional(dir.path()).is_err(),
            "a directory is an error"
        );
    }

    #[test]
    fn writes_create_parents_and_leave_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/config.json");
        write_atomically(&path, "{}\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomically(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn backups_sit_next_to_the_file_with_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "original").unwrap();
        let backup = back_up(&path).unwrap();
        assert_eq!(backup.parent(), path.parent());
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mcp.json.gitrs-backup-")
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "original");
        let second = back_up(&path).unwrap();
        assert_ne!(second, backup, "the first backup is kept");
        assert!(back_up(&dir.path().join("none")).is_err());
    }
}
