//! The repo's verify commands, run by the daemon in a build's worktree. The agent may have
//! changed what they run, so in `workspace` access they get the agent's confinement: no writes
//! outside the workspace folder. macOS confines them with `sandbox-exec`, Linux with
//! `bwrap` (bubblewrap); elsewhere `workspace` access is refused when the daemon starts.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mergepatrol_resolver::{Access, VerifyFailed};

/// The longest one verify command may take.
const TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How much of a failed command's output the agent is shown.
const TAIL: usize = 6000;

/// Whether this machine can confine the verify commands to `access`. An error says what is
/// missing.
pub async fn check(access: &Access) -> Result<()> {
    let Access::Workspace(_) = access else {
        return Ok(());
    };
    let (program, arg) = if cfg!(target_os = "macos") {
        (
            "sandbox-exec",
            ["-p", "(version 1)(allow default)", "/usr/bin/true"].as_slice(),
        )
    } else if cfg!(target_os = "linux") {
        ("bwrap", ["--ro-bind", "/", "/", "/bin/true"].as_slice())
    } else {
        bail!(
            "`workspace` access confines the verify commands on macOS and Linux only: set `access = \"full\"`"
        );
    };
    let ok = tokio::process::Command::new(program)
        .args(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !ok {
        bail!(
            "`workspace` access runs the verify commands under `{program}`, which does not run here{}: install it, or set `access = \"full\"`",
            if program == "bwrap" {
                " (bubblewrap)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// Runs each command in `wt`, in order, until one fails: that one and the end of its output.
pub async fn run(
    commands: &[String],
    wt: &Path,
    access: &Access,
    tmp: &Path,
) -> Result<Option<VerifyFailed>> {
    tokio::fs::create_dir_all(tmp).await?;
    for command in commands {
        let mut cmd = shell(command, access);
        cmd.current_dir(wt)
            .env("TMPDIR", tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
            Ok(out) => out.with_context(|| format!("run `{command}`"))?,
            Err(_) => {
                return Ok(Some(VerifyFailed {
                    command: command.clone(),
                    output: format!("It did not finish in {} minutes.", TIMEOUT.as_secs() / 60),
                }));
            }
        };
        if !out.status.success() {
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            output.push_str(&String::from_utf8_lossy(&out.stderr));
            return Ok(Some(VerifyFailed {
                command: command.clone(),
                output: tail(&output),
            }));
        }
    }
    Ok(None)
}

/// `sh -c command`, confined to the workspace folder in `workspace` access.
fn shell(command: &str, access: &Access) -> tokio::process::Command {
    let Access::Workspace(folder) = access else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
        return cmd;
    };
    let folder = folder.to_string_lossy();
    if cfg!(target_os = "macos") {
        let quoted = folder.replace('\\', "\\\\").replace('"', "\\\"");
        let profile = format!(
            "(version 1)(allow default)(deny file-write*)\
             (allow file-write* (subpath \"{quoted}\") (literal \"/dev/null\") (literal \"/dev/zero\") \
             (literal \"/dev/dtracehelper\") (regex #\"^/dev/tty\") (subpath \"/dev/fd\"))"
        );
        let mut cmd = tokio::process::Command::new("sandbox-exec");
        cmd.args(["-p", &profile, "sh", "-c", command]);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("bwrap");
        cmd.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
            .args([
                "--bind",
                &folder,
                &folder,
                "--die-with-parent",
                "sh",
                "-c",
                command,
            ]);
        cmd
    }
}

/// The last `TAIL` bytes of `s`, cut at a character.
fn tail(s: &str) -> String {
    let mut start = s.len().saturating_sub(TAIL);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_first_failing_command_is_named_with_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let commands = vec![
            "echo fine".to_owned(),
            "echo broken >&2; exit 3".to_owned(),
            "touch never".to_owned(),
        ];
        let failed = run(
            &commands,
            dir.path(),
            &Access::Full,
            &dir.path().join("tmp"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(failed.command, "echo broken >&2; exit 3");
        assert_eq!(failed.output.trim(), "broken");
        assert!(!dir.path().join("never").exists());
        assert!(
            run(
                &commands[..1],
                dir.path(),
                &Access::Full,
                &dir.path().join("tmp")
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn a_long_output_keeps_its_end() {
        let long = format!("{}é{}", "a".repeat(10_000), "end");
        let t = tail(&long);
        assert!(t.ends_with("éend") && t.len() <= TAIL);
        // A cut inside a character moves past it.
        let t = tail(&format!("é{}", "a".repeat(TAIL - 1)));
        assert_eq!(t, "a".repeat(TAIL - 1));
    }

    /// Writing inside the workspace folder works, outside it fails.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn in_workspace_access_a_command_cannot_write_outside_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (ws, outside) = (root.join("ws"), root.join("outside"));
        std::fs::create_dir_all(ws.join("wt")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let access = Access::Workspace(ws.clone());
        check(&access).await.unwrap();
        let inside = vec!["echo x > made && echo y > \"$TMPDIR/t\"".to_owned()];
        let wt = ws.join("wt");
        assert!(
            run(&inside, &wt, &access, &ws.join("tmp"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(wt.join("made").exists() && ws.join("tmp/t").exists());

        let escape = vec![format!("echo x > {}/escaped", outside.display())];
        let failed = run(&escape, &wt, &access, &ws.join("tmp")).await.unwrap();
        assert!(failed.is_some());
        assert!(!outside.join("escaped").exists());
    }
}
