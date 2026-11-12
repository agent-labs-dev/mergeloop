//! One build, from a claimed task to its open PR: the agent builds the issue in a fresh
//! worktree, the verify commands pass, the daemon commits as the App's bot with the developer as
//! co-author, pushes with a minted token, and has the server open the PR. However it ends, the
//! worktree and branch go, and the server hears how it ended.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use mergepatrol_git::Git;
use mergepatrol_resolver::{
    Access, BuildRequest, LocalAgent, Outcome, Resolution, VerifyFailed, build_description,
    build_prompt, commit_subject,
};
use tokio::sync::watch;

use super::folder::{self, Folder};
use super::lane::{Lane, Stage, Task};
use super::verify;
use crate::client::{Refusal, needs_sign_in, refusal};

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
}

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
}

/// What the work itself came to, before the server hears of it.
#[derive(Debug)]
enum Work {
    Opened(u64),
    Stuck(String),
}

/// The daemon was stopped.
#[derive(Debug)]
struct Stopped;

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
        if let Err(e) = self.folder.remove(task).await {
            say(task, &format!("could not clean up: {e:#}"));
        }
        let _ = self
            .folder
            .prune_transcripts(std::time::SystemTime::now())
            .await;
        ended
    }

    async fn work(&self, lane: &Lane, task: &Task) -> Result<Work> {
        lane.heartbeat(task, Stage::Building).await?;
        let token = mint(lane, task).await?;
        let wt = self.folder.checkout(task, &token).await?;
        let base = folder::rev_parse(&wt, "HEAD").await?;
        say(task, &format!("building in {}", wt.display()));
        let run = match self.change(task, &wt).await? {
            Ok(run) => run,
            Err(why) => return Ok(Work::Stuck(why)),
        };
        // Ticket 07's review of the change goes here, before anything is pushed.
        publish(lane, task, &wt, &base, &run).await
    }

    /// The agent's change, which the verify commands pass: the agent's last run that names its
    /// commit. `Err` with why it is stuck when there is none.
    async fn change(&self, task: &Task, wt: &Path) -> Result<Result<Resolution, String>> {
        let run = self.run_agent(task, wt, None, "build").await?;
        if let Some(why) = stuck_by(&run.outcome) {
            return Ok(Err(why));
        }
        let tmp = self.folder.tmp();
        let Some(failed) = verify::run(&task.verify, wt, &self.access, &tmp).await? else {
            return Ok(Ok(run));
        };
        say(
            task,
            &format!("`{}` failed: the agent fixes it", failed.command),
        );
        let fix = self.run_agent(task, wt, Some(&failed), "fix").await?;
        if let Some(why) = stuck_by(&fix.outcome) {
            return Ok(Err(why));
        }
        if let Some(failed) = verify::run(&task.verify, wt, &self.access, &tmp).await? {
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
        wt: &Path,
        failed: Option<&VerifyFailed>,
        step: &str,
    ) -> Result<Resolution> {
        let prompt = build_prompt(&BuildRequest {
            repo: &task.repo,
            issue: task.issue,
            title: &task.title,
            body: &task.body,
            verify: &task.verify,
            failed,
        });
        let transcript = self
            .folder
            .transcripts(task.build)
            .join(format!("{step}.jsonl"));
        let run = self
            .agent
            .run(wt, &prompt, &self.access, &self.folder.tmp(), &transcript)
            .await?;
        say(
            task,
            &format!("the agent's {step} run ended: {:?}", run.outcome),
        );
        Ok(run)
    }
}

/// Why the build is stuck, when the agent's run did not end with a change.
fn stuck_by(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Resolved => None,
        Outcome::GaveUp(why) => Some(format!("The agent gave up: {why}")),
        Outcome::Failed(why) => Some(format!("The agent failed: {why}")),
    }
}

/// Commits the change as the App's bot, pushes it with a fresh token (the first may be near the
/// end of its hour), and has the server open the PR.
async fn publish(
    lane: &Lane,
    task: &Task,
    wt: &Path,
    base: &str,
    run: &Resolution,
) -> Result<Work> {
    let subject = commit_subject(&run.text)
        .unwrap_or_else(|| format!("Build #{}: {}", task.issue, task.title));
    if !folder::commit(wt, base, &subject, &task.author, &task.co_author).await? {
        return Ok(Work::Stuck("The agent changed nothing.".into()));
    }
    let head = folder::rev_parse(wt, "HEAD").await?;
    let token = mint(lane, task).await?;
    Git.push_branch(wt, &token, &task.clone_url, &head, &task.branch)
        .await?;
    say(task, &format!("pushed {}", task.branch));
    let pr = lane
        .open_pr(task, &subject, &build_description(&run.text))
        .await?;
    say(task, &format!("opened PR #{pr}"));
    Ok(Work::Opened(pr))
}

/// What the server must hear of how the work ended.
#[derive(Debug)]
enum Settle {
    /// Nothing: it opened its PR, or it no longer has the build for this daemon.
    Nothing(Ended),
    /// `release_build`, stuck for the reason or given back.
    Release(Option<String>, Ended),
    /// The sign-in is no good: the daemon must stop.
    Fatal(anyhow::Error),
}

fn settlement(work: Result<Work>) -> Settle {
    let e = match work {
        Ok(Work::Opened(pr)) => return Settle::Nothing(Ended::Opened(pr)),
        Ok(Work::Stuck(why)) => return Settle::Release(Some(shorten(&why)), Ended::Stuck(why)),
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
        _ => Settle::Release(Some(shorten(&said)), Ended::Stuck(said)),
    }
}

/// Tells the server how the work ended, and says how the build ended.
async fn settle(lane: &Lane, task: &Task, work: Result<Work>) -> Result<Ended> {
    let (stuck, ended) = match settlement(work) {
        Settle::Nothing(ended) => return Ok(ended),
        Settle::Fatal(e) => {
            say(task, &format!("{e:#}"));
            return Err(e);
        }
        Settle::Release(stuck, ended) => (stuck, ended),
    };
    match lane.release(task, stuck.as_deref()).await {
        Err(e) if needs_sign_in(&e) => return Err(e),
        Err(e) => say(task, &format!("could not release it: {e:#}")),
        Ok(()) => {}
    }
    Ok(ended)
}

/// A token for the build's repo, heartbeating first when the server went too long without one.
async fn mint(lane: &Lane, task: &Task) -> Result<String> {
    match lane.mint(task).await {
        Err(e) if refusal(&e) == Some(Refusal::Unheard) => {
            lane.heartbeat(task, Stage::Building).await?;
            lane.mint(task).await
        }
        minted => minted,
    }
}

/// Heartbeats the build every `every`, until a heartbeat fails: that error.
async fn heartbeats(lane: &Lane, task: &Task, every: Duration) -> anyhow::Error {
    loop {
        tokio::time::sleep(every).await;
        if let Err(e) = lane.heartbeat(task, Stage::Building).await {
            return e;
        }
    }
}

/// Waits until `stop` turns true, or its sender goes.
async fn stopped(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

fn shorten(why: &str) -> String {
    if why.len() <= REASON_MAX {
        return why.to_owned();
    }
    let mut end = REASON_MAX;
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &why[..end])
}

fn say(task: &Task, what: &str) {
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
