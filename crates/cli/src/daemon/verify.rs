//! The repo's verify commands, run by the daemon in a build's tree. The agent may have changed
//! what they run, so in `workspace` access they get the agent's confinement: writes only in the
//! build's room, and no reads of the denied paths, through a symlink either. macOS confines them
//! with `sandbox-exec`, which matches a path after following its symlinks, Linux with `bwrap`
//! (bubblewrap), which hides each denied path that exists under an empty one; elsewhere
//! `workspace` access is refused when the daemon starts.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mergepatrol_resolver::{Access, Room, VerifyFailed};

/// The longest one verify command may take.
const TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How much of a failed command's output the agent is shown.
const TAIL: usize = 6000;

/// Whether this machine can confine the verify commands to `access`. An error says what is
/// missing.
pub async fn check(access: &Access) -> Result<()> {
    let Access::Workspace { .. } = access else {
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

/// Runs each command in the room's tree, in order, until one fails: that one and the end of its
/// output.
pub async fn run(
    commands: &[String],
    room: &Room,
    access: &Access,
) -> Result<Option<VerifyFailed>> {
    tokio::fs::create_dir_all(room.tmp()).await?;
    for command in commands {
        let mut cmd = shell(command, room, access);
        cmd.current_dir(&room.tree)
            .env("TMPDIR", room.tmp())
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

/// `sh -c command`, confined to the room in `workspace` access.
fn shell(command: &str, room: &Room, access: &Access) -> tokio::process::Command {
    let Access::Workspace { deny_read } = access else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
        return cmd;
    };
    if cfg!(target_os = "macos") {
        let mut cmd = tokio::process::Command::new("sandbox-exec");
        cmd.arg("-p")
            .arg(profile(&room.root, deny_read))
            .args(["sh", "-c", command]);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("bwrap");
        cmd.args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--bind",
        ])
        .arg(&room.root)
        .arg(&room.root);
        for path in deny_read {
            match std::fs::metadata(path) {
                Ok(m) if m.is_dir() => _ = cmd.arg("--tmpfs").arg(path),
                Ok(_) => _ = cmd.args(["--ro-bind", "/dev/null"]).arg(path),
                // What is not there cannot be read, nor made there: the room is all it writes.
                Err(_) => {}
            }
        }
        cmd.args(["--die-with-parent", "sh", "-c", command]);
        cmd
    }
}

/// The `sandbox-exec` profile: writes only in `root` and to the terminal devices, and no reads
/// of `deny_read`.
fn profile(root: &Path, deny_read: &[std::path::PathBuf]) -> String {
    let quote = |p: &Path| {
        let p = p.to_string_lossy();
        format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\""))
    };
    let mut profile = format!(
        "(version 1)(allow default)(deny file-write*)\
         (allow file-write* (subpath {}) (literal \"/dev/null\") (literal \"/dev/zero\") \
         (literal \"/dev/dtracehelper\") (regex #\"^/dev/tty\") (subpath \"/dev/fd\"))",
        quote(root)
    );
    for path in deny_read {
        profile.push_str(&format!("(deny file-read* (subpath {}))", quote(path)));
    }
    profile
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

    fn room(dir: &Path) -> Room {
        Room {
            root: dir.to_owned(),
            tree: dir.to_owned(),
        }
    }

    #[tokio::test]
    async fn the_first_failing_command_is_named_with_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let commands = vec![
            "echo fine".to_owned(),
            "echo broken >&2; exit 3".to_owned(),
            "touch never".to_owned(),
        ];
        let room = room(dir.path());
        let failed = run(&commands, &room, &Access::Full).await.unwrap().unwrap();
        assert_eq!(failed.command, "echo broken >&2; exit 3");
        assert_eq!(failed.output.trim(), "broken");
        assert!(!dir.path().join("never").exists());
        assert!(
            run(&commands[..1], &room, &Access::Full)
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

    /// In the room it writes; beside it, and outside, it does not; a denied path it does not
    /// read, nor a symlink to it.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn in_workspace_access_a_command_writes_only_its_room_and_reads_nothing_denied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let room = Room {
            root: root.join("ws/builds/1"),
            tree: root.join("ws/builds/1/tree"),
        };
        let (sibling, decoy) = (root.join("ws/builds/2"), root.join("decoy"));
        for d in [&room.tree, &sibling, &decoy] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(decoy.join("secret"), "DECOY").unwrap();
        std::os::unix::fs::symlink(decoy.join("secret"), room.tree.join("link")).unwrap();
        let access = Access::workspace(std::slice::from_ref(&decoy));
        check(&access).await.unwrap();
        let inside = vec!["echo x > made && echo y > \"$TMPDIR/t\"".to_owned()];
        assert!(run(&inside, &room, &access).await.unwrap().is_none());
        assert!(room.tree.join("made").exists() && room.tmp().join("t").exists());

        for (command, left) in [
            (
                format!("echo x > {}/escaped", sibling.display()),
                sibling.join("escaped"),
            ),
            (
                format!("echo x > {}/escaped", root.display()),
                root.join("escaped"),
            ),
        ] {
            assert!(run(&[command], &room, &access).await.unwrap().is_some());
            assert!(!left.exists());
        }
        for read in [
            format!("cat {}/secret", decoy.display()),
            "cat link".to_owned(),
        ] {
            let failed = run(std::slice::from_ref(&read), &room, &access)
                .await
                .unwrap();
            let failed = failed.unwrap_or_else(|| panic!("{read} read the decoy"));
            assert!(
                !failed.output.contains("DECOY"),
                "{read}: {}",
                failed.output
            );
        }
    }
}
