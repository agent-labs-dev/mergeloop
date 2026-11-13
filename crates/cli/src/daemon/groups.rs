//! The process groups of running builds, noted in the daemon's own folder, `daemon/groups/`,
//! out of every build's reach: one file `<build>.<pgid>` per agent run or verify command, holding
//! when the group's leader started. A build's groups are stopped when each run ends and when the
//! build ends, before its room goes; on start, the daemon stops the groups a crash left.
//!
//! A process that leaves its group (`setsid`, `setpgid`) is not found this way.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mergepatrol_resolver::Group;

use super::folder::not_followed;

/// The daemon's note of the process groups its builds run.
#[derive(Clone, Debug)]
pub struct Groups {
    dir: PathBuf,
}

impl Groups {
    pub fn new(dir: PathBuf) -> Self {
        Groups { dir }
    }

    /// Notes that build `build` runs the process group led by `pgid`.
    pub fn track(&self, build: u64, pgid: u32) {
        let noted = std::fs::create_dir_all(&self.dir)
            .and_then(|()| std::fs::write(self.dir.join(format!("{build}.{pgid}")), started(pgid)));
        if let Err(e) = noted {
            eprintln!("mergeloop: build {build}: could not note process group {pgid}: {e}");
        }
    }

    /// Stops every process of the build's groups, waits for them to go, and forgets them.
    pub async fn stop(&self, build: u64) -> Result<()> {
        self.stop_where(|b| b == build).await
    }

    /// Stops the groups of every build: what a daemon that died mid-build left running.
    pub async fn stop_all(&self) -> Result<()> {
        self.stop_where(|_| true).await
    }

    /// A record it cannot list or remove is an error, and the records are kept: what they name
    /// may still run.
    async fn stop_where(&self, which: impl Fn(u64) -> bool) -> Result<()> {
        let Some(entries) = self.records()? else {
            return Ok(());
        };
        for entry in entries {
            let path = entry?.path();
            let Some((build, group)) = parse(&path) else {
                continue;
            };
            if !which(build) {
                continue;
            }
            let noted = std::fs::read_to_string(&path).unwrap_or_default();
            stop(group, noted.trim()).await;
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("remove {}", path.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The records, `None` when there are none. Only a file where their folder goes, which
    /// records nothing, is removed.
    fn records(&self) -> Result<Option<std::fs::ReadDir>> {
        match std::fs::symlink_metadata(&self.dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Ok(m) if m.is_symlink() => return Err(not_followed(&self.dir)),
            Ok(m) if !m.is_dir() => {
                eprintln!(
                    "mergeloop: removes {}: a file where the process-group records go",
                    self.dir.display()
                );
                std::fs::remove_file(&self.dir)
                    .with_context(|| format!("remove {}", self.dir.display()))?;
                return Ok(None);
            }
            _ => {}
        }
        let entries = std::fs::read_dir(&self.dir).with_context(|| {
            format!(
                "list {}, the record of what builds run: what a crash left running may still run. Give it back its permissions (chmod u+rwx) and start again",
                self.dir.display()
            )
        })?;
        Ok(Some(entries))
    }
}

/// `<build>.<pgid>`.
fn parse(path: &Path) -> Option<(u64, Group)> {
    let name = path.file_name()?.to_str()?;
    let (build, pgid) = name.split_once('.')?;
    Some((build.parse().ok()?, Group::led_by(pgid.parse().ok()?)?))
}

/// When process `pid` started, as `ps` says: empty when it is gone.
fn started(pid: u32) -> String {
    std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Kills the group led by `pgid` and waits for it to go, if it is still the build's: its leader
/// is gone or a zombie (a pid is not given again while its group lives), or is the one that
/// started at `noted`. A leader that started at another time is a stranger with a reused pid.
async fn stop(group: Group, noted: &str) {
    if !group.exists() {
        return;
    }
    let leader = if group.leader_exists() {
        started(group.leader())
    } else {
        String::new()
    };
    if !leader.is_empty() && leader != noted {
        return;
    }
    group.stop().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process group of its own, `sh` with a `sleep` child, reaped as it dies.
    fn group() -> Group {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 60 & sleep 60"]);
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().unwrap();
        let group = Group::led_by(child.id()).unwrap();
        std::thread::spawn(move || child.wait());
        group
    }

    #[test]
    fn only_a_build_and_a_group_of_its_own_are_read_from_a_name() {
        assert_eq!(
            parse(Path::new("/g/7.4242")),
            Some((7, Group::led_by(4242).unwrap()))
        );
        for name in ["7.1", "7.0", "7.-5", "x.4242", "74242"] {
            assert_eq!(parse(&Path::new("/g").join(name)), None, "{name}");
        }
    }

    #[tokio::test]
    async fn a_stopped_group_is_gone_when_stop_returns() {
        let dir = tempfile::tempdir().unwrap();
        let groups = Groups::new(dir.path().join("groups"));
        let group = group();
        groups.track(3, group.leader());
        groups.stop(3).await.unwrap();
        assert!(!group.exists(), "the group outlived stop");
        assert_eq!(
            std::fs::read_dir(dir.path().join("groups"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn a_group_whose_leader_started_at_another_time_is_a_stranger_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let groups = Groups::new(dir.path().join("groups"));
        let group = group();
        std::fs::create_dir_all(dir.path().join("groups")).unwrap();
        let noted = dir.path().join(format!("groups/4.{}", group.leader()));
        std::fs::write(&noted, "Mon Jan  1 00:00:00 2001").unwrap();
        groups.stop_all().await.unwrap();
        assert!(group.exists(), "a stranger was stopped");
        assert!(!noted.exists(), "it is forgotten all the same");
        group.stop().await;
    }
}
