//! The process groups of running builds, noted in the daemon's own folder, `daemon/groups/`,
//! out of every build's reach: one file `<build>.<pgid>` per agent run or verify command, holding
//! when the group's leader started. A build's groups are stopped when each run ends and when the
//! build ends, before its room goes; on start, the daemon stops the groups a crash left.
//!
//! A process that leaves its group (`setsid`, `setpgid`) is not found this way.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// The longest a stopped group is waited for.
const STOP_WAIT: Duration = Duration::from_secs(10);

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

    async fn stop_where(&self, which: impl Fn(u64) -> bool) -> Result<()> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.dir.display())),
        };
        for entry in entries {
            let path = entry?.path();
            let Some((build, pgid)) = parse(&path) else {
                continue;
            };
            if !which(build) {
                continue;
            }
            let noted = std::fs::read_to_string(&path).unwrap_or_default();
            stop(pgid, noted.trim()).await;
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        Ok(())
    }
}

/// `<build>.<pgid>`.
fn parse(path: &Path) -> Option<(u64, i32)> {
    let name = path.file_name()?.to_str()?;
    let (build, pgid) = name.split_once('.')?;
    Some((build.parse().ok()?, pgid.parse().ok().filter(|p| *p > 1)?))
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
async fn stop(pgid: i32, noted: &str) {
    if !exists(-pgid) {
        return;
    }
    let leader = if exists(pgid) {
        started(pgid as u32)
    } else {
        String::new()
    };
    if !leader.is_empty() && leader != noted {
        return;
    }
    // SAFETY: `kill` takes any pid and signal; a negative pid names a process group.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    while exists(-pgid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether a process (`pid` > 0) or a process group (`-pgid`) exists.
fn exists(pid: i32) -> bool {
    // SAFETY: signal 0 checks only.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
