//! `mergeloop daemon`: builds the build lane's tasks of every workspace the signed-in developer
//! belongs to, as many at once as it has slots, until it is stopped. Each task is claimed, built
//! by a local agent CLI on the developer's own login, verified, pushed as the App's bot, and its
//! PR opened by the server.
//!
//! - `config`: the config file.
//! - `lane`: the build lane's tools, for one workspace.
//! - `folder`: the workspace folder: clones, worktrees, transcripts, and their cleanup.
//! - `verify`: the repo's verify commands, confined like the agent.
//! - `build`: one build, from a claimed task to its open PR.

pub mod build;
pub mod config;
pub mod folder;
pub mod lane;
pub mod verify;

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::client::{Client, Refusal, needs_sign_in, refusal};
use build::{Builder, Ended};
use config::Config;
use folder::Folder;
use lane::{Lane, Task};

/// The daemon, with how often it does what.
pub struct Daemon {
    pub config: Config,
    pub client: Client,
    /// How long a workspace with nothing to build waits before the next claim.
    pub poll: Duration,
    /// How often a build heartbeats.
    pub heartbeat_every: Duration,
}

impl Daemon {
    pub fn new(config: Config, client: Client) -> Self {
        Daemon {
            config,
            client,
            poll: Duration::from_secs(30),
            heartbeat_every: Duration::from_secs(5 * 60),
        }
    }

    /// Builds until `stop` resolves, then gives back what it holds and cleans up. An error when
    /// it cannot start, or when the sign-in is no good.
    pub async fn run(self, stop: impl Future<Output = ()>) -> Result<()> {
        let access = self.config.access();
        verify::check(&access).await?;
        let agent = &self.config.builder;
        eprintln!(
            "mergeloop: daemon: {} slot(s), builder `{}` ({}), {} access, in {}",
            self.config.slots,
            agent.command,
            agent.vendor.name(),
            match self.config.access {
                config::AccessMode::Workspace => "workspace",
                config::AccessMode::Full => "full",
            },
            self.config.workspace.display()
        );
        for caveat in access.caveats(agent.vendor) {
            eprintln!("mergeloop: daemon: note: {caveat}");
        }
        let (folder, _lock) = Folder::open(&self.config.workspace)?;
        folder.reconcile(&[]).await?;
        let workspaces = lane::workspaces(&self.client).await?;
        if workspaces.is_empty() {
            bail!(
                "your sign-in belongs to no workspace: install MergeLoop on a GitHub account first"
            );
        }
        let lanes: Vec<Lane> = workspaces
            .into_iter()
            .map(|workspace| Lane {
                client: self.client.clone(),
                workspace,
                daemon: folder.id().to_owned(),
            })
            .collect();
        for lane in &lanes {
            give_back_held(lane).await?;
        }
        let builder = Builder {
            agent: self.config.builder.clone(),
            access,
            folder,
            heartbeat_every: self.heartbeat_every,
        };
        Slots {
            lanes,
            builder,
            slots: self.config.slots,
            poll: self.poll,
        }
        .run(stop)
        .await
    }
}

/// Gives back the builds without a PR that this daemon holds from before it stopped: their work
/// went with it. A build with a PR is the land loop's. Another daemon's builds, of the same login
/// too, are not listed.
async fn give_back_held(lane: &Lane) -> Result<()> {
    let held = match lane.held().await {
        Ok(held) => held,
        Err(e) if refusal(&e) == Some(Refusal::LaneOff) => return Ok(()),
        Err(e) => return Err(e),
    };
    for build in held.iter().filter(|b| b.pr.is_none()) {
        match lane.give_back(build).await {
            Ok(()) => eprintln!("mergeloop: gave back build {}, left from before", build.id),
            Err(e) if needs_sign_in(&e) => return Err(e),
            Err(e) => eprintln!("mergeloop: could not give back build {}: {e:#}", build.id),
        }
    }
    Ok(())
}

/// When the claim loop next asks for a task: when the first workspace wakes, or a poll from now
/// when none sleeps. Never while every slot is taken: a build ending wakes it then.
fn wake(asleep: &HashMap<u64, Instant>, free: bool, poll: Duration) -> Option<Instant> {
    free.then(|| {
        asleep
            .values()
            .min()
            .copied()
            .unwrap_or_else(|| Instant::now() + poll)
    })
}

/// Sleeps until `wake`, or forever.
async fn sleep_until(wake: Option<Instant>) {
    match wake {
        Some(wake) => tokio::time::sleep_until(wake).await,
        None => std::future::pending().await,
    }
}

/// What the claim loop keeps between its turns.
struct State {
    builds: JoinSet<Result<Ended>>,
    /// When each workspace may be asked for a task again.
    asleep: HashMap<u64, Instant>,
    /// The workspace to ask first next time.
    next: usize,
    failed: Option<anyhow::Error>,
}

/// The claim loop: fills free slots with tasks, round the workspaces.
struct Slots {
    lanes: Vec<Lane>,
    builder: Builder,
    slots: usize,
    poll: Duration,
}

impl Slots {
    async fn run(self, stop: impl Future<Output = ()>) -> Result<()> {
        let (stopping, stop_rx) = watch::channel(false);
        let mut state = State {
            builds: JoinSet::new(),
            asleep: HashMap::new(),
            next: 0,
            failed: None,
        };
        tokio::pin!(stop);
        loop {
            if !*stopping.borrow()
                && let Err(e) = self.fill(&mut state, &stop_rx).await
            {
                state.failed = Some(e);
                stopping.send_replace(true);
            }
            if *stopping.borrow() && state.builds.is_empty() {
                break;
            }
            let wake = wake(&state.asleep, state.builds.len() < self.slots, self.poll);
            tokio::select! {
                Some(done) = state.builds.join_next() => match done {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        state.failed.get_or_insert(e);
                        stopping.send_replace(true);
                    }
                    Err(e) => eprintln!("mergeloop: a build panicked: {e}"),
                },
                () = sleep_until(wake), if !*stopping.borrow() => {}
                () = &mut stop, if !*stopping.borrow() => {
                    eprintln!("mergeloop: stopping: giving back what it holds");
                    stopping.send_replace(true);
                }
            }
        }
        state.failed.map_or(Ok(()), Err)
    }

    /// Claims tasks round the workspaces that are awake, until every slot is taken or none is
    /// awake. An error only when the sign-in is no good.
    async fn fill(&self, state: &mut State, stop: &watch::Receiver<bool>) -> Result<()> {
        while state.builds.len() < self.slots {
            let now = Instant::now();
            let Some(lane) = (0..self.lanes.len())
                .map(|i| &self.lanes[(state.next + i) % self.lanes.len()])
                .find(|l| state.asleep.get(&l.workspace).is_none_or(|t| *t <= now))
            else {
                return Ok(());
            };
            state.next = (state.next + 1) % self.lanes.len();
            match self.claim(lane).await {
                Ok(Some(task)) => {
                    let (builder, lane, stop) = (self.builder.clone(), lane.clone(), stop.clone());
                    state
                        .builds
                        .spawn(async move { builder.build(&lane, &task, stop).await });
                }
                Ok(None) => _ = state.asleep.insert(lane.workspace, now + self.poll),
                Err(e) if needs_sign_in(&e) => return Err(e),
                Err(e) => {
                    eprintln!("mergeloop: workspace {}: {e:#}", lane.workspace);
                    state.asleep.insert(lane.workspace, now + self.poll);
                }
            }
        }
        Ok(())
    }

    /// A task to build from `lane`, or none for now. A task that already has its PR is given
    /// back: landing it is not this daemon's yet.
    async fn claim(&self, lane: &Lane) -> Result<Option<Task>> {
        let task = match lane.claim().await {
            Ok(task) => task,
            Err(e) if refusal(&e) == Some(Refusal::LaneOff) => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(task) = task else {
            return Ok(None);
        };
        if let Some(pr) = task.pr {
            eprintln!(
                "mergeloop: build {} ({}#{}) has PR #{pr}: giving it back, landing is not built yet",
                task.build, task.repo, task.issue
            );
            lane.release(&task, None).await?;
            return Ok(None);
        }
        eprintln!(
            "mergeloop: build {} ({}#{}): claimed: {}",
            task.build, task.repo, task.issue, task.title
        );
        Ok(Some(task))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_every_slot_taken_a_passed_deadline_does_not_wake_the_loop() {
        let poll = Duration::from_secs(30);
        let passed = Instant::now() - Duration::from_secs(5);
        let asleep = HashMap::from([(2, passed)]);
        assert_eq!(wake(&asleep, false, poll), None);
        assert_eq!(wake(&asleep, true, poll), Some(passed));
        let none = wake(&HashMap::new(), true, poll).unwrap();
        assert!(none > Instant::now() + poll - Duration::from_secs(1));
    }
}
