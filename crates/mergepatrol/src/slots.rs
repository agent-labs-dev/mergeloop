//! The sandbox's job slots, which the jobs of every repo share. A job is for one head of its PR,
//! and may wait a long while for a slot. Once the PR moves, what the job does is thrown away. So a
//! job reads the PR's head again once it has its slot, and a push to the PR, as a webhook or a
//! tick tells of it, stops the job, waiting or running. A stopped job's future is dropped, which
//! frees its sandbox, and its slot.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use mergepatrol_core::{PrNumber, RepoName};
use mergepatrol_store::LaneKey;
use parking_lot::Mutex;
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;

use crate::github::GitHub;

type PrId = (u64, RepoName, PrNumber);

/// How soon a job reads GitHub again for a head it was told of before. Every tick tells it of
/// the head it sees; a webhook can come before GitHub shows the push.
const READ_AGAIN: Duration = Duration::from_secs(60);

/// The PR moved off the head its job was for, so the job stopped, or never started: a job for
/// the new head takes its place.
#[derive(Debug)]
pub struct Outdated(String);

impl std::fmt::Display for Outdated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = self.0.get(..8).unwrap_or(&self.0);
        write!(f, "the PR moved off {short} before its job was done")
    }
}

impl std::error::Error for Outdated {}

impl Outdated {
    /// Whether `e`, or what it wraps, is a job its PR moved away from.
    pub fn caused(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.is::<Outdated>())
    }
}

pub struct JobSlots {
    slots: Semaphore,
    /// The jobs waiting or running, by PR.
    watched: Mutex<HashMap<PrId, Vec<Watch>>>,
    next: AtomicU64,
}

struct Watch {
    token: u64,
    /// The head the job is for.
    head: String,
    /// The last other head it was told of, and when it read GitHub for it.
    told: Option<(String, Instant)>,
    moved: Arc<Notify>,
}

impl JobSlots {
    pub fn new(slots: usize) -> Self {
        Self {
            slots: Semaphore::new(slots),
            watched: Mutex::default(),
            next: AtomicU64::new(0),
        }
    }

    /// Runs `job` on a slot once one is free, for as long as PR `number` stays at `head`, which
    /// the job fetches from `head_ref`. `Err(Outdated)` once it moved, which drops `job`.
    pub(crate) async fn run<T>(
        &self,
        gh: &dyn GitHub,
        key: &LaneKey,
        number: PrNumber,
        (head_ref, head): (&str, &str),
        job: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let pr = Head {
            gh,
            repo: &key.repo,
            number,
            head_ref,
            head,
        };
        let watching = self.watch((key.installation_id, key.repo.clone(), number), pr.head);
        let _slot = tokio::select! {
            slot = self.slots.acquire() => slot?,
            outdated = pr.moved(&watching.moved) => return Err(outdated.into()),
        };
        // The wait may have been long, and a push since may have gone untold.
        if !pr.still().await {
            return Err(pr.outdated().into());
        }
        tokio::select! {
            ran = job => ran,
            outdated = pr.moved(&watching.moved) => Err(outdated.into()),
        }
    }

    /// PR `number` of `repo` is at `head`, as a webhook or a tick saw: its jobs for another head
    /// read GitHub for theirs, and stop if it moved.
    pub(crate) fn saw(&self, installation: u64, repo: &RepoName, number: PrNumber, head: &str) {
        let mut watched = self.watched.lock();
        let Some(jobs) = watched.get_mut(&(installation, repo.clone(), number)) else {
            return;
        };
        let read_lately = |w: &Watch| {
            w.told
                .as_ref()
                .is_some_and(|(h, at)| h == head && at.elapsed() < READ_AGAIN)
        };
        for w in jobs
            .iter_mut()
            .filter(|w| w.head != head && !read_lately(w))
        {
            w.told = Some((head.to_owned(), Instant::now()));
            w.moved.notify_one();
        }
    }

    /// Slots free now, for tests.
    #[cfg(test)]
    pub(crate) fn free(&self) -> usize {
        self.slots.available_permits()
    }

    /// Takes a slot, as another repo's job would, for tests.
    #[cfg(test)]
    pub(crate) fn take(&self) -> tokio::sync::SemaphorePermit<'_> {
        self.slots.try_acquire().expect("a slot is free")
    }

    /// Jobs waiting or running on PR `number`, for tests.
    #[cfg(test)]
    pub(crate) fn watched(&self, installation: u64, repo: &RepoName, number: PrNumber) -> usize {
        let watched = self.watched.lock();
        let jobs = watched.get(&(installation, repo.clone(), number));
        jobs.map_or(0, Vec::len)
    }

    fn watch(&self, id: PrId, head: &str) -> Watching<'_> {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let moved = Arc::new(Notify::new());
        let watch = Watch {
            token,
            head: head.to_owned(),
            told: None,
            moved: moved.clone(),
        };
        self.watched
            .lock()
            .entry(id.clone())
            .or_default()
            .push(watch);
        Watching {
            slots: self,
            id,
            token,
            moved,
        }
    }
}

/// A job's watch on its PR, which ends with the job, also if its future is dropped.
struct Watching<'a> {
    slots: &'a JobSlots,
    id: PrId,
    token: u64,
    moved: Arc<Notify>,
}

impl Drop for Watching<'_> {
    fn drop(&mut self) {
        let mut watched = self.slots.watched.lock();
        if let Some(jobs) = watched.get_mut(&self.id) {
            jobs.retain(|w| w.token != self.token);
            if jobs.is_empty() {
                watched.remove(&self.id);
            }
        }
    }
}

/// The head of a PR that a job is for, and where GitHub shows the PR's head now.
struct Head<'a> {
    gh: &'a dyn GitHub,
    repo: &'a RepoName,
    number: PrNumber,
    /// The full ref the job fetches: the PR's branch, or for a fork PR, its pull ref.
    head_ref: &'a str,
    head: &'a str,
}

impl Head<'_> {
    /// Whether the PR is still at the head. One read of GitHub.
    async fn still(&self) -> bool {
        let now = match self.head_ref.strip_prefix("refs/heads/") {
            Some(branch) => self.gh.branch_sha(self.repo, branch).await,
            None => self.gh.pull_head(self.repo, self.number).await,
        };
        match now {
            Ok(now) => now == self.head,
            // No reason to stop it: the job finds out for itself, as it would without this read.
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(repo = %self.repo, pr = self.number, %error, "could not read the PR's head for its job");
                true
            }
        }
    }

    /// Ends once a push is told of that moved the PR. A head told of is read from GitHub: a
    /// webhook may come late, or twice.
    async fn moved(&self, told: &Notify) -> Outdated {
        loop {
            told.notified().await;
            if !self.still().await {
                return self.outdated();
            }
        }
    }

    fn outdated(&self) -> Outdated {
        Outdated(self.head.to_owned())
    }
}
