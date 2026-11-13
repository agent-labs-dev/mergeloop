//! A process group: what an agent run or a verify command started, which it may leave running.

use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, kill_process_group, test_kill_process, test_kill_process_group,
};

/// The longest a stopped group is waited for.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// A process group, named by the pid of the process that leads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group(Pid);

impl Group {
    /// The group led by `pid`: none for pid 0, which is no process, for 1, whose group is the
    /// system's, or for a pid past what a pid can be.
    pub fn led_by(pid: u32) -> Option<Group> {
        let pid = i32::try_from(pid).ok().filter(|p| *p > 1)?;
        Pid::from_raw(pid).map(Group)
    }

    /// The pid of its leader.
    pub fn leader(self) -> u32 {
        self.0.as_raw_nonzero().get().unsigned_abs()
    }

    /// Whether a process of the group is left.
    pub fn exists(self) -> bool {
        found(test_kill_process_group(self.0))
    }

    /// Whether its leader is left, a zombie included.
    pub fn leader_exists(self) -> bool {
        found(test_kill_process(self.0))
    }

    /// Kills every process of the group and waits until none is left, for at most `STOP_WAIT`.
    pub async fn stop(self) {
        // An error means none is left that it may kill, which `exists` then tells.
        let _ = kill_process_group(self.0, Signal::KILL);
        let deadline = tokio::time::Instant::now() + STOP_WAIT;
        while self.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// A signal that may not be sent still found its process.
fn found(test: rustix::io::Result<()>) -> bool {
    matches!(test, Ok(()) | Err(Errno::PERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group of its own, `sh` with a `sleep` child, reaped as it dies.
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
    fn no_group_is_led_by_no_process_init_or_a_pid_past_range() {
        for pid in [0, 1, u32::MAX] {
            assert_eq!(Group::led_by(pid), None, "{pid}");
        }
        assert_eq!(Group::led_by(4242).map(Group::leader), Some(4242));
    }

    #[tokio::test]
    async fn a_stopped_group_is_gone_when_stop_returns_and_not_waited_on_after() {
        let group = group();
        assert!(group.exists() && group.leader_exists());
        let started = std::time::Instant::now();
        group.stop().await;
        assert!(!group.exists(), "the group outlived stop");
        assert!(
            started.elapsed() < STOP_WAIT,
            "stop waited out its deadline"
        );
    }

    #[test]
    fn a_process_it_may_not_signal_still_exists() {
        // Init, which only root may signal.
        assert!(found(test_kill_process(Pid::INIT)));
    }
}
