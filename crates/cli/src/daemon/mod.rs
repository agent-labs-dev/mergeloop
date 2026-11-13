//! `mergeloop daemon`: builds the build lane's tasks of every workspace the signed-in developer
//! belongs to, as many at once as it has slots, until it is stopped. Each task is claimed, built
//! by a local agent CLI on the developer's own login, verified, pushed as the App's bot, and its
//! PR opened by the server.
//!
//! - `config`: the config file.
//! - `lane`: the build lane's tools, for one workspace.
//! - `folder`: the workspace folder: clones, worktrees, transcripts, and their cleanup.
//! - `groups`: the process groups builds run, stopped with them.
//! - `verify`: the repo's verify commands, confined like the agent.
//! - `env`: what a build's agent and commands may act as: never the developer.
//! - `build`: one build, from a claimed task to its open PR.
//! - `landing`: a built PR, from its opening until it merges.
//! - `github`: what the daemon says on a PR's threads, as the App's bot.

pub mod build;
pub mod config;
pub mod env;
pub mod folder;
pub mod github;
pub mod groups;
pub mod landing;
pub mod lane;
pub mod verify;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::client::{Client, Refusal, needs_sign_in, refusal};
use build::{Builder, Ended};
use config::Config;
use folder::Folder;
use landing::Entry;
use lane::{Lane, Task};

/// The daemon, with how often it does what.
pub struct Daemon {
    pub config: Config,
    pub client: Client,
    /// How long a workspace with nothing to build waits before the next claim.
    pub poll: Duration,
    /// How often a build heartbeats.
    pub heartbeat_every: Duration,
    /// GitHub's API, where the daemon replies on threads as the App's bot.
    pub github: String,
    /// Runs in each build once its room is made: for tests.
    pub in_room: Option<build::InRoom>,
}

impl Daemon {
    pub fn new(config: Config, client: Client) -> Self {
        Daemon {
            config,
            client,
            poll: Duration::from_secs(30),
            heartbeat_every: Duration::from_secs(5 * 60),
            github: "https://api.github.com".into(),
            in_room: None,
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
        let reviewer = self.config.reviewer.as_ref();
        match reviewer {
            Some(r) => eprintln!(
                "mergeloop: daemon: reviewer `{}` ({}), read-only",
                r.command,
                r.vendor.name()
            ),
            None => eprintln!("mergeloop: daemon: no review before a build is pushed"),
        }
        let mut caveats = access.caveats(agent.vendor);
        for caveat in reviewer
            .map(|r| access.caveats(r.vendor))
            .unwrap_or_default()
        {
            if !caveats.contains(&caveat) {
                caveats.push(caveat);
            }
        }
        for caveat in caveats {
            eprintln!("mergeloop: daemon: note: {caveat}");
        }
        let (folder, _lock) = Folder::open(&self.config.workspace)?;
        // What a crash left running goes before what it left on disk.
        folder.groups().stop_all().await;
        folder.reconcile(&[]).await;
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
        let mut resumed = Vec::new();
        for lane in &lanes {
            resumed.extend(take_up_held(lane, self.poll).await?);
        }
        let builder = Builder {
            agent: self.config.builder.clone(),
            reviewer: self.config.reviewer.clone(),
            access,
            folder,
            heartbeat_every: self.heartbeat_every,
            pause: self.poll,
            slots: Arc::new(Semaphore::new(self.config.slots)),
            http: crate::client::http()?,
            github: self.github,
            in_room: self.in_room,
        };
        Slots {
            lanes,
            builder,
            poll: self.poll,
        }
        .run(resumed, stop)
        .await
    }
}

/// Takes up the builds this daemon holds from before it stopped: those with a PR, to land them
/// again, and gives back those without, whose work went with it. Another daemon's builds, of the
/// same login too, are not listed.
async fn take_up_held(lane: &Lane, pause: Duration) -> Result<Vec<(Lane, Task)>> {
    let said = |e: &anyhow::Error| eprintln!("mergeloop: could not list the builds held: {e:#}");
    let held = match lane::answered(pause, said, || lane.held()).await {
        Ok(held) => held,
        Err(e) if refusal(&e) == Some(Refusal::LaneOff) => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut resumed = Vec::new();
    for build in &held {
        let taken = match build.pr {
            Some(_) => {
                let said = |e: &anyhow::Error| {
                    eprintln!("mergeloop: could not read build {}'s task: {e:#}", build.id);
                };
                lane::answered(pause, said, || lane.task_of(build))
                    .await
                    .map(|task| {
                        eprintln!(
                            "mergeloop: lands build {}'s PR again, from before",
                            build.id
                        );
                        resumed.push((lane.clone(), task));
                    })
            }
            None => lane.give_back(build).await.map(|()| {
                eprintln!("mergeloop: gave back build {}, left from before", build.id);
            }),
        };
        match taken {
            Ok(()) => {}
            Err(e) if needs_sign_in(&e) => return Err(e),
            Err(e) => eprintln!("mergeloop: could not take up build {}: {e:#}", build.id),
        }
    }
    Ok(resumed)
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

/// What a build task that failed said: its panic's message.
fn panicked(e: tokio::task::JoinError) -> String {
    match e.try_into_panic() {
        Ok(panic) => panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "it panicked".into()),
        Err(e) => e.to_string(),
    }
}

/// What the claim loop keeps between its turns.
struct State {
    /// The builds and the landings. A build holds its slot; a landing takes one only while its
    /// agent works a need.
    builds: JoinSet<Result<Ended>>,
    /// The build each task runs, to settle it should the task fail, or land its PR once open.
    running: HashMap<tokio::task::Id, (Lane, Task)>,
    /// When each workspace may be asked for a task again.
    asleep: HashMap<u64, Instant>,
    /// The workspace to ask first next time.
    next: usize,
    failed: Option<anyhow::Error>,
}

/// The claim loop: fills free slots with tasks, round the workspaces, and lands the PRs they
/// open.
struct Slots {
    lanes: Vec<Lane>,
    builder: Builder,
    poll: Duration,
}

impl Slots {
    async fn run(self, resumed: Vec<(Lane, Task)>, stop: impl Future<Output = ()>) -> Result<()> {
        let (stopping, stop_rx) = watch::channel(false);
        let mut state = State {
            builds: JoinSet::new(),
            running: HashMap::new(),
            asleep: HashMap::new(),
            next: 0,
            failed: None,
        };
        for (lane, task) in resumed {
            self.land(&mut state, &stop_rx, lane, task, Entry::TakenUp);
        }
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
            let slots = &self.builder.slots;
            let free = slots.available_permits() > 0;
            let wake = wake(&state.asleep, free, self.poll);
            tokio::select! {
                Some(done) = state.builds.join_next_with_id() => {
                    let ended = match done {
                        Ok((id, ended)) => {
                            let ran = state.running.remove(&id);
                            match (ended, ran) {
                                (Ok(Ended::Opened(pr)), Some((lane, mut task))) => {
                                    task.pr = Some(pr);
                                    self.land(&mut state, &stop_rx, lane, task, Entry::Opened);
                                    Ok(())
                                }
                                (ended, _) => ended.map(drop),
                            }
                        }
                        Err(e) => match state.running.remove(&e.id()) {
                            Some((lane, task)) => {
                                let why = panicked(e);
                                self.builder.failed(&lane, &task, &why).await
                            }
                            None => Ok(()),
                        },
                    };
                    if let Err(e) = ended {
                        state.failed.get_or_insert(e);
                        stopping.send_replace(true);
                    }
                }
                () = sleep_until(wake), if !*stopping.borrow() => {}
                // A landing's agent gave its slot back.
                _ = slots.acquire(), if !free && !*stopping.borrow() => {}
                () = &mut stop, if !*stopping.borrow() => {
                    eprintln!("mergeloop: stopping: giving back the builds without a PR");
                    stopping.send_replace(true);
                }
            }
        }
        state.failed.map_or(Ok(()), Err)
    }

    /// Lands the PR of `task` from `lane`, holding no slot while it waits.
    fn land(
        &self,
        state: &mut State,
        stop: &watch::Receiver<bool>,
        lane: Lane,
        task: Task,
        entry: Entry,
    ) {
        let (builder, stop) = (self.builder.clone(), stop.clone());
        let (l, t) = (lane.clone(), task.clone());
        let spawned = state
            .builds
            .spawn(async move { builder.land(&l, &t, entry, stop).await });
        state.running.insert(spawned.id(), (lane, task));
    }

    /// Claims tasks round the workspaces that are awake, until every slot is taken or none is
    /// awake. An error only when the sign-in is no good.
    async fn fill(&self, state: &mut State, stop: &watch::Receiver<bool>) -> Result<()> {
        while let Ok(slot) = self.builder.slots.clone().try_acquire_owned() {
            let now = Instant::now();
            let Some(lane) = (0..self.lanes.len())
                .map(|i| &self.lanes[(state.next + i) % self.lanes.len()])
                .find(|l| state.asleep.get(&l.workspace).is_none_or(|t| *t <= now))
            else {
                return Ok(());
            };
            state.next = (state.next + 1) % self.lanes.len();
            match self.claim(lane).await {
                // Claimed with its PR, from a holder that went quiet or gave it back.
                Ok(Some(task)) if task.pr.is_some() => {
                    self.land(state, stop, lane.clone(), task, Entry::TakenUp)
                }
                Ok(Some(task)) => {
                    let (builder, stop) = (self.builder.clone(), stop.clone());
                    let (l, t) = (lane.clone(), task.clone());
                    let spawned = state.builds.spawn(async move {
                        let _slot = slot;
                        builder.build(&l, &t, stop).await
                    });
                    state.running.insert(spawned.id(), (lane.clone(), task));
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

    /// A task to build, or to land, from `lane`, or none for now.
    async fn claim(&self, lane: &Lane) -> Result<Option<Task>> {
        let task = match lane.claim().await {
            Ok(task) => task,
            Err(e) if refusal(&e) == Some(Refusal::LaneOff) => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(task) = task else {
            return Ok(None);
        };
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
