//! One build, from a claimed task to its open PR: the agent builds the issue in a fresh
//! worktree, the verify commands pass, the daemon commits as the App's bot with the developer as
//! co-author, pushes with a minted token, and has the server open the PR. However it ends, the
//! worktree and branch go, and the server hears how it ended.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use mergepatrol_git::Git;
use mergepatrol_resolver::{
    Access, BuildRequest, Env, LocalAgent, Outcome, Resolution, VerifyFailed, build_description,
    build_prompt, commit_subject,
};
use tokio::sync::{Semaphore, watch};

use super::env;
use super::folder::{Checkout, Folder};
use super::lane::{Lane, Stage, Task};
use super::verify;
use crate::client::{Refusal, Refused, needs_sign_in, refusal};

/// How many times `open_build_pr` is called again when it says to.
const OPEN_RETRIES: u32 = 3;

/// The longest reason a stuck build gives: it goes on the issue as a comment.
const REASON_MAX: usize = 3000;

/// What a build runs with.
#[derive(Clone)]
pub struct Builder {
    pub folder: Folder,
    pub agent: LocalAgent,
    pub access: Access,
    /// How often a build tells the server it is alive. The server lets 30 minutes pass.
    pub heartbeat_every: Duration,
    /// How long to wait before calling again what said to, or what failed.
    pub pause: Duration,
    /// The slots: an agent runs only while it holds one.
    pub slots: Arc<Semaphore>,
    /// For GitHub's API, as the App's bot.
    pub http: reqwest::Client,
    /// GitHub's API: `https://api.github.com`, or a test's.
    pub github: String,
    /// Runs in each build once its room is made: for tests.
    pub in_room: Option<InRoom>,
}

/// A step a test puts into each build.
pub type InRoom = std::sync::Arc<dyn Fn(&Task) + Send + Sync>;

/// How a build ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// Its PR is open. Landing it is the land loop's.
    Opened(u64),
    /// Released as stuck, for this reason.
    Stuck(String),
    /// Given back, for this reason: anyone may build it.
    GivenBack(String),
    /// Ended without a word to the server, which no longer has it for this daemon.
    Dropped(String),
    /// Its PR merged.
    Merged,
}

/// What the work itself came to, before the server hears of it.
#[derive(Debug)]
pub(super) enum Work {
    Opened(u64),
    Stuck(String),
    Merged,
}

/// The daemon was stopped.
#[derive(Debug)]
pub(super) struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the daemon was stopped")
    }
}

impl std::error::Error for Stopped {}

impl Builder {
    /// Builds `task` until its PR is open, or it ends otherwise, or `stop` turns true. An error
    /// only when the sign-in is no good: nothing more can be done until the developer signs in.
    pub async fn build(
        &self,
        lane: &Lane,
        task: &Task,
        stop: watch::Receiver<bool>,
    ) -> Result<Ended> {
        let work = tokio::select! {
            work = self.work(lane, task) => work,
            e = heartbeats(lane, task, self.heartbeat_every) => Err(e),
            () = stopped(stop) => Err(Stopped.into()),
        };
        let ended = settle(lane, task, work).await;
        self.clean_up(task).await;
        ended
    }

    /// Settles a build whose task failed, as when the daemon has a bug: released as stuck, saying
    /// so, and cleaned up. An error only when the sign-in is no good.
    pub async fn failed(&self, lane: &Lane, task: &Task, why: &str) -> Result<()> {
        let doing = match task.stage() {
            Stage::Building => "building it",
            Stage::Landing => "landing its PR",
        };
        let why = format!("The daemon failed while {doing}: {why}");
        say(task, &why);
        let settled = settle(lane, task, Ok(Work::Stuck(why))).await;
        self.clean_up(task).await;
        settled.map(drop)
    }

    /// Stops what the build left running, then removes its room and branch.
    pub async fn clean_up(&self, task: &Task) {
        if let Err(e) = self.folder.groups().stop(task.build).await {
            say(task, &format!("could not stop what it left running: {e:#}"));
        }
        if let Err(e) = self.folder.remove(task).await {
            say(task, &format!("could not clean up: {e:#}"));
        }
        let _ = self
            .folder
            .prune_transcripts(std::time::SystemTime::now())
            .await;
    }

    async fn work(&self, lane: &Lane, task: &Task) -> Result<Work> {
        lane.heartbeat(task, Stage::Building).await?;
        let token = heard(lane, task, || lane.mint(task)).await?;
        let checkout = self.folder.checkout(task, &token, &self.access).await?;
        if let Some(in_room) = &self.in_room {
            in_room(task);
        }
        let base = Git.tree_head(&checkout.tree).await?;
        say(
            task,
            &format!("building in {}", checkout.room.tree.display()),
        );
        let prompt = |failed: Option<&VerifyFailed>| {
            build_prompt(&BuildRequest {
                repo: &task.repo,
                issue: task.issue,
                title: &task.title,
                body: &task.body,
                verify: &task.verify,
                failed,
            })
        };
        let env = env::nobody(&checkout.room);
        let run = match self.change(task, &checkout, prompt, &env, "build").await? {
            Ok(run) => run,
            Err(why) => return Ok(Work::Stuck(why)),
        };
        // Ticket 07's review of the change goes here, before anything is pushed.
        self.publish(lane, task, &checkout, &base, &run).await
    }

    /// The agent's change, which the verify commands pass: the agent's last run that names its
    /// commit. `prompt` is the agent's, given what failed in verify after its run, if anything
    /// did. `Err` with why it is stuck when there is none.
    pub(super) async fn change(
        &self,
        task: &Task,
        checkout: &Checkout,
        prompt: impl Fn(Option<&VerifyFailed>) -> String,
        env: &Env,
        step: &str,
    ) -> Result<Result<Resolution, String>> {
        let room = &checkout.room;
        let run = self
            .run_agent(task, checkout, &prompt(None), env, step)
            .await?;
        if let Some(why) = stuck_by(&run.outcome) {
            return Ok(Err(why));
        }
        let groups = self.folder.groups();
        let nobody = env::nobody(room);
        let verify = || {
            verify::run(
                &task.verify,
                room,
                &self.access,
                &nobody,
                &groups,
                task.build,
            )
        };
        let Some(failed) = verify().await? else {
            return Ok(Ok(run));
        };
        say(
            task,
            &format!("`{}` failed: the agent fixes it", failed.command),
        );
        let fix = self
            .run_agent(
                task,
                checkout,
                &prompt(Some(&failed)),
                env,
                &format!("{step}-fix"),
            )
            .await?;
        if let Some(why) = stuck_by(&fix.outcome) {
            return Ok(Err(why));
        }
        if let Some(failed) = verify().await? {
            return Ok(Err(format!(
                "`{}` still fails after the agent's fix:\n```\n{}\n```",
                failed.command,
                failed.output.trim_end()
            )));
        }
        Ok(Ok(if commit_subject(&fix.text).is_some() {
            fix
        } else {
            run
        }))
    }

    async fn run_agent(
        &self,
        task: &Task,
        checkout: &Checkout,
        prompt: &str,
        env: &Env,
        step: &str,
    ) -> Result<Resolution> {
        let transcript = self
            .folder
            .transcripts(task.build)
            .join(format!("{step}.jsonl"));
        let groups = self.folder.groups();
        let spawned = |pid| groups.track(task.build, pid);
        let run = self
            .agent
            .run(
                &checkout.room,
                prompt,
                &self.access,
                env,
                &transcript,
                spawned,
            )
            .await;
        // What the agent left running ends with its run.
        groups.stop(task.build).await?;
        let run = run?;
        say(
            task,
            &format!("the agent's {step} run ended: {:?}", run.outcome),
        );
        Ok(run)
    }
}

/// Why the build is stuck, when the agent's run did not end with a change.
pub(super) fn stuck_by(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Resolved => None,
        Outcome::GaveUp(why) => Some(format!("The agent gave up: {why}")),
        Outcome::Failed(why) => Some(format!("The agent failed: {why}")),
    }
}

impl Builder {
    /// Commits the change as the App's bot, pushes it with a fresh token (the first may be near
    /// the end of its hour), and has the server open the PR.
    async fn publish(
        &self,
        lane: &Lane,
        task: &Task,
        checkout: &Checkout,
        base: &str,
        run: &Resolution,
    ) -> Result<Work> {
        let subject = commit_subject(&run.text)
            .unwrap_or_else(|| format!("Build #{}: {}", task.issue, task.title));
        let (bot, dev) = (&task.author, &task.co_author);
        let message = format!(
            "{subject}\n\nCo-authored-by: {} <{}>\n",
            dev.name, dev.email
        );
        let author = (bot.name.as_str(), bot.email.as_str());
        if !Git.squash(&checkout.tree, base, &message, author).await? {
            return Ok(Work::Stuck("The agent changed nothing.".into()));
        }
        let head = Git.tree_head(&checkout.tree).await?;
        let token = heard(lane, task, || lane.mint(task)).await?;
        if !push(task, checkout, &token, &head).await? {
            return Ok(Work::Stuck(format!(
                "The branch {} changed on GitHub while the build ran, so the build did not push \
             over it. Look at what is on it, then put the label on again to build anew.",
                task.branch
            )));
        }
        say(task, &format!("pushed {}", task.branch));
        let description = build_description(&run.text);
        let mut tries = 0;
        let pr = loop {
            match heard(lane, task, || lane.open_pr(task, &subject, &description)).await {
                Err(e) if tries < OPEN_RETRIES && call_again(&e) => {
                    tries += 1;
                    say(task, &format!("{e:#}: calling again"));
                    tokio::time::sleep(self.pause).await;
                    lane.heartbeat(task, Stage::Building).await?;
                }
                opened => break opened?,
            }
        };
        say(task, &format!("opened PR #{pr}"));
        Ok(Work::Opened(pr))
    }
}

/// Pushes `head` to the task's branch with `token`, leased on where the checkout saw the branch.
/// `false` when the branch moved on GitHub since: that is someone's work, never pushed over.
pub(super) async fn push(
    task: &Task,
    checkout: &Checkout,
    token: &str,
    head: &str,
) -> Result<bool> {
    let seen = checkout.remote.as_deref().unwrap_or_default();
    let (clone, branch) = (&checkout.clone, &task.branch);
    if let Err(e) = Git
        .push_leased(clone, token, &task.clone_url, head, branch, seen)
        .await
    {
        if Git.remote_head(clone, token, branch).await? != checkout.remote {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(true)
}

/// Whether the server says to call again: GitHub refused part of it, or limits for now.
fn call_again(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>()
        .is_some_and(|r| matches!(r.status, Some(502 | 503)))
}

/// What the server must hear of how the work ended.
#[derive(Debug)]
enum Settle {
    /// Nothing: it opened its PR, or it no longer has the build for this daemon.
    Nothing(Ended),
    /// `release_build`, stuck for the reason or given back.
    Release(Option<String>, Ended),
    /// `report_build` `merged`: the PR merged.
    Merged,
    /// The sign-in is no good: the daemon must stop.
    Fatal(anyhow::Error),
}

fn settlement(work: Result<Work>) -> Settle {
    let e = match work {
        Ok(Work::Opened(pr)) => return Settle::Nothing(Ended::Opened(pr)),
        Ok(Work::Stuck(why)) => return Settle::Release(Some(shorten(&why)), Ended::Stuck(why)),
        Ok(Work::Merged) => return Settle::Merged,
        Err(e) => e,
    };
    let said = format!("{e:#}");
    if e.is::<Stopped>() {
        return Settle::Release(None, Ended::GivenBack(said));
    }
    if needs_sign_in(&e) {
        return Settle::Fatal(e);
    }
    match refusal(&e) {
        Some(Refusal::Lost | Refusal::LaneOff) => Settle::Nothing(Ended::Dropped(said)),
        Some(Refusal::Cancelled | Refusal::MayNotPush) => {
            Settle::Release(None, Ended::GivenBack(said))
        }
        Some(Refusal::Merged) => Settle::Merged,
        _ => Settle::Release(Some(shorten(&said)), Ended::Stuck(said)),
    }
}

/// Tells the server how the work ended, and says how the build ended.
pub(super) async fn settle(lane: &Lane, task: &Task, work: Result<Work>) -> Result<Ended> {
    let (stuck, ended) = match settlement(work) {
        Settle::Nothing(ended) => return Ok(ended),
        Settle::Fatal(e) => {
            say(task, &format!("{e:#}"));
            return Err(e);
        }
        Settle::Release(stuck, ended) => (stuck, ended),
        Settle::Merged => {
            match lane.report_merged(task).await {
                Err(e) if needs_sign_in(&e) => return Err(e),
                Err(e) => say(task, &format!("could not report it merged: {e:#}")),
                Ok(()) => {}
            }
            return Ok(Ended::Merged);
        }
    };
    match lane.release(task, stuck.as_deref()).await {
        Err(e) if needs_sign_in(&e) => return Err(e),
        Err(e) => say(task, &format!("could not release it: {e:#}")),
        Ok(()) => {}
    }
    Ok(ended)
}

/// `call`'s answer, heartbeating and asking once more when the server went too long without a
/// heartbeat: it acts only for a live claim.
pub(super) async fn heard<T, F: std::future::Future<Output = Result<T>>>(
    lane: &Lane,
    task: &Task,
    call: impl Fn() -> F,
) -> Result<T> {
    match call().await {
        Err(e) if refusal(&e) == Some(Refusal::Unheard) => {
            lane.heartbeat(task, task.stage()).await?;
            call().await
        }
        answered => answered,
    }
}

/// Heartbeats the build every `every`, until a heartbeat fails: that error.
pub(super) async fn heartbeats(lane: &Lane, task: &Task, every: Duration) -> anyhow::Error {
    loop {
        tokio::time::sleep(every).await;
        if let Err(e) = lane.heartbeat(task, task.stage()).await {
            return e;
        }
    }
}

/// Waits until `stop` turns true, or its sender goes.
pub(super) async fn stopped(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

pub(super) fn shorten(why: &str) -> String {
    if why.len() <= REASON_MAX {
        return why.to_owned();
    }
    let mut end = REASON_MAX;
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &why[..end])
}

pub(super) fn say(task: &Task, what: &str) {
    eprintln!(
        "mergeloop: build {} ({}#{}): {what}",
        task.build, task.repo, task.issue
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Refused;

    fn refused(status: u64, kind: Option<Refusal>) -> anyhow::Error {
        Refused {
            tool: "heartbeat_build".into(),
            status: Some(status),
            kind,
            text: "no".into(),
        }
        .into()
    }

    fn release(work: Result<Work>) -> Option<Option<String>> {
        match settlement(work) {
            Settle::Release(stuck, _) => Some(stuck),
            _ => None,
        }
    }

    #[test]
    fn each_ending_tells_the_server_what_it_must_hear() {
        assert!(matches!(
            settlement(Ok(Work::Opened(5))),
            Settle::Nothing(Ended::Opened(5))
        ));
        assert_eq!(
            release(Ok(Work::Stuck("unclear".into()))),
            Some(Some("unclear".into()))
        );
        assert_eq!(release(Err(Stopped.into())), Some(None));
        for kind in [Refusal::Cancelled, Refusal::MayNotPush] {
            assert_eq!(
                release(Err(refused(409, Some(kind)))),
                Some(None),
                "{kind:?}"
            );
        }
        for kind in [Refusal::Lost, Refusal::LaneOff] {
            assert!(matches!(
                settlement(Err(refused(409, Some(kind)))),
                Settle::Nothing(Ended::Dropped(_))
            ));
        }
        assert!(matches!(
            settlement(Err(refused(409, Some(Refusal::Merged)))),
            Settle::Merged
        ));
        assert!(matches!(
            settlement(Err(refused(401, None))),
            Settle::Fatal(_)
        ));
        let stuck = release(Err(anyhow::anyhow!("git push: denied")))
            .unwrap()
            .unwrap();
        assert!(stuck.contains("git push: denied"), "{stuck}");
    }

    #[test]
    fn a_long_reason_is_cut_at_a_character() {
        let long = format!("{}é", "a".repeat(REASON_MAX - 1));
        let cut = shorten(&long);
        assert!(cut.ends_with('…') && cut.len() <= REASON_MAX + '…'.len_utf8());
        assert_eq!(shorten("short"), "short");
    }
}
