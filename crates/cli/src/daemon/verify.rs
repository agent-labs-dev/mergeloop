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

use super::groups::Groups;

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
/// output. Each runs in a process group of its own, noted in `groups` for `build`, and whatever
/// it leaves running is stopped when it ends.
pub async fn run(
    commands: &[String],
    room: &Room,
    access: &Access,
    groups: &Groups,
    build: u64,
) -> Result<Option<VerifyFailed>> {
    tokio::fs::create_dir_all(room.tmp()).await?;
    for command in commands {
        let mut cmd = shell(command, room, access);
        cmd.current_dir(&room.tree)
            .env("TMPDIR", room.tmp())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("run `{command}`"))?;
        if let Some(pid) = child.id() {
            groups.track(build, pid);
        }
        let (stdout, stderr) = (read(child.stdout.take()), read(child.stderr.take()));
        let waited = tokio::time::timeout(TIMEOUT, child.wait()).await;
        // Ends what it left running, which may hold its output open.
        groups.stop(build).await?;
        let status = match waited {
            Ok(status) => status.with_context(|| format!("run `{command}`"))?,
            Err(_) => {
                return Ok(Some(VerifyFailed {
                    command: command.clone(),
                    output: format!("It did not finish in {} minutes.", TIMEOUT.as_secs() / 60),
                }));
            }
        };
        if !status.success() {
            let mut output = stdout.await.unwrap_or_default();
            output.push_str(&stderr.await.unwrap_or_default());
            return Ok(Some(VerifyFailed {
                command: command.clone(),
                output: tail(&output),
            }));
        }
    }
    Ok(None)
}

/// All of `pipe`, read as it comes, so a full pipe never blocks the command.
fn read(
    pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            use tokio::io::AsyncReadExt;
            let _ = pipe.read_to_end(&mut bytes).await;
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
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

    /// A note of process groups, in a folder of its own.
    fn groups() -> (tempfile::TempDir, Groups) {
        let dir = tempfile::tempdir().unwrap();
        let groups = Groups::new(dir.path().join("groups"));
        (dir, groups)
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
        let (_g, groups) = groups();
        let failed = run(&commands, &room, &Access::Full, &groups, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.command, "echo broken >&2; exit 3");
        assert_eq!(failed.output.trim(), "broken");
        assert!(!dir.path().join("never").exists());
        assert!(
            run(&commands[..1], &room, &Access::Full, &groups, 1)
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
        let (_g, groups) = groups();
        let confined = |commands: &[String]| {
            let commands = commands.to_vec();
            let (room, access, groups) = (room.clone(), access.clone(), groups.clone());
            async move { run(&commands, &room, &access, &groups, 1).await }
        };
        let inside = vec!["echo x > made && echo y > \"$TMPDIR/t\"".to_owned()];
        assert!(confined(&inside).await.unwrap().is_none());
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
            assert!(confined(&[command]).await.unwrap().is_some());
            assert!(!left.exists());
        }
        for read in [
            format!("cat {}/secret", decoy.display()),
            "cat link".to_owned(),
        ] {
            let failed = confined(std::slice::from_ref(&read)).await.unwrap();
            let failed = failed.unwrap_or_else(|| panic!("{read} read the decoy"));
            assert!(
                !failed.output.contains("DECOY"),
                "{read}: {}",
                failed.output
            );
        }
    }
}
