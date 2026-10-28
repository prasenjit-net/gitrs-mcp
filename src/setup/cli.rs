//! Running an agent's own command line (only `claude`), with a fixed argument list and a timeout.

use std::{
    path::Path,
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::Duration,
};

use anyhow::Context;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Run `program args...` in `cwd`, never through a shell, and give up after thirty seconds.
pub(crate) fn run(program: &Path, args: &[String], cwd: &Path) -> anyhow::Result<Output> {
    let (sender, receiver) = mpsc::channel();
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The child is waited for on its own thread, so a hung agent can't hang setup.
    std::thread::spawn(move || {
        let _ = sender.send(command.output());
    });
    receiver
        .recv_timeout(TIMEOUT)
        .map_err(|_| {
            anyhow::anyhow!(
                "`{}` didn't finish within {}s",
                program.display(),
                TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("cannot run {}", program.display()))
}

/// The output's error text for a message, or its exit status when it printed none.
pub(crate) fn failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = stderr.trim();
    if text.is_empty() {
        output.status.to_string()
    } else {
        text.to_owned()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let ok = run(Path::new("/bin/echo"), &["hi".into()], dir.path()).unwrap();
        assert!(ok.status.success() && ok.stdout == b"hi\n");
        let bad = run(
            Path::new("/bin/sh"),
            &["-c".into(), "echo oops >&2; exit 3".into()],
            dir.path(),
        )
        .unwrap();
        assert_eq!(failure(&bad), "oops");
        let silent = run(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 3".into()],
            dir.path(),
        )
        .unwrap();
        assert!(failure(&silent).contains('3'));
        assert!(run(&dir.path().join("missing"), &[], dir.path()).is_err());
    }
}
