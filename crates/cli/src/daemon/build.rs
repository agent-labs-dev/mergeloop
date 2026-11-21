//! One build, from a claimed task to its open PR: the agent builds the issue in a fresh
//! worktree, the verify commands pass, the daemon commits as the App's bot with the developer as
//! co-author, pushes with a minted token, and has the server open the PR. However it ends, the
//! worktree and branch go, and the server hears how it ended.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use mergepatrol_git::Git;
use mergepatrol_resolver::{
    Access, BuildFinding, BuildRequest, BuildReview, Env, LocalAgent, Outcome, Resolution, Room,
    VerifyFailed, build_description, build_prompt, commit_subject, review_build_findings,
    review_build_prompt,
};
use tokio::sync::{Semaphore, watch};

use super::env;
use super::folder::{Checkout, Folder};
use super::lane::{Lane, Scope, Stage, Task};
use super::verify;
use crate::client::{Refusal, needs_sign_in, refusal, unanswered};

/// The most times the reviewer reads a build's change. Findings left after the last make the
/// build stuck.
pub(super) const REVIEW_ROUNDS: u32 = 3;

/// The longest reason a stuck build gives: it goes on the issue as a comment.
const REASON_MAX: usize = 3000;

/// What a build runs with.
#[derive(Clone)]
pub struct Builder {
    pub folder: Folder,
    pub agent: LocalAgent,
    /// Reads each build's change before it is pushed, when set.
    pub reviewer: Option<LocalAgent>,
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
        let (stage, staged) = watch::channel(Stage::Building);
        let work = tokio::select! {
            work = self.work(lane, task, &stage) => work,
            e = heartbeats(lane, task, || *staged.borrow(), self.heartbeat_every) => Err(e),
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
            Stage::Building | Stage::Reviewing => "building it",
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

    async fn work(&self, lane: &Lane, task: &Task, stage: &watch::Sender<Stage>) -> Result<Work> {
        self.beat(lane, task, Stage::Building).await?;
        let token = self.mint(lane, task, Scope::Push).await?;
        let checkout = self.folder.checkout(task, &token, &self.access).await?;
        if let Some(in_room) = &self.in_room {
            in_room(task);
        }
        let base = Git.tree_head(&checkout.tree).await?;
        say(
            task,
            &format!("building in {}", checkout.room.tree.display()),
        );
        match self.rounds(lane, task, &checkout, &base, stage).await? {
            Ok((built, reviewed)) => {
                self.publish(lane, task, &checkout, &built, reviewed.as_ref())
                    .await
            }
            Err(why) => Ok(Work::Stuck(why)),
        }
    }

    /// The change, committed on `base`, that the reviewer asks nothing more of: the builder's
    /// run, verified, then each round's review, with the findings back to the builder, at most
    /// `REVIEW_ROUNDS` times. With it, the run that says what the PR is, and how the review
    /// went. `Err` with why the build is stuck.
    async fn rounds(
        &self,
        lane: &Lane,
        task: &Task,
        checkout: &Checkout,
        base: &str,
        stage: &watch::Sender<Stage>,
    ) -> Result<Result<(Resolution, Option<Reviewed>), String>> {
        let env = env::nobody(&checkout.room);
        let mut findings: Vec<BuildFinding> = vec![];
        let mut built: Option<Resolution> = None;
        let mut fixed = 0;
        for round in 1..=REVIEW_ROUNDS {
            let prompt = |failed: Option<&VerifyFailed>| {
                build_prompt(&BuildRequest {
                    repo: &task.repo,
                    issue: task.issue,
                    title: &task.title,
                    body: &task.body,
                    verify: &task.verify,
                    failed,
                    findings: &findings,
                })
            };
            let step = match round {
                1 => "build".to_owned(),
                n => format!("build-{n}"),
            };
            let run = match self.change(task, checkout, prompt, &env, &step).await? {
                Ok(run) => run,
                Err(why) => return Ok(Err(why)),
            };
            // The first run built the issue: its subject and description say what the PR is.
            let run = match built.take() {
                Some(first) if commit_subject(&first.text).is_some() => first,
                _ => run,
            };
            let Some(head) = commit(task, checkout, base, &run).await? else {
                return Ok(Err("The agent changed nothing.".into()));
            };
            let Some(reviewer) = &self.reviewer else {
                return Ok(Ok((run, None)));
            };
            stage.send_replace(Stage::Reviewing);
            self.beat(lane, task, Stage::Reviewing).await?;
            let review = self.review(task, reviewer, checkout, base, &head, round);
            findings = match review.await? {
                Ok(findings) => findings,
                Err(why) => return Ok(Err(why)),
            };
            stage.send_replace(Stage::Building);
            if findings.is_empty() {
                let by = reviewer.clone();
                let reviewed = Reviewed {
                    by,
                    rounds: round,
                    fixed,
                };
                return Ok(Ok((run, Some(reviewed))));
            }
            say(
                task,
                &format!("the reviewer asks for {} change(s)", findings.len()),
            );
            fixed += findings.len();
            built = Some(run);
            self.beat(lane, task, Stage::Building).await?;
        }
        Ok(Err(unreviewed(&findings)))
    }

    /// What the reviewer asks of the change at `head`, in a fresh session of its own that can
    /// read the build's tree but not write it. `Err` with why the build is stuck when it did not
    /// review, or when the tree is not as it was: then nothing it did is pushed.
    async fn review(
        &self,
        task: &Task,
        reviewer: &LocalAgent,
        checkout: &Checkout,
        base: &str,
        head: &str,
        round: u32,
    ) -> Result<Result<Vec<BuildFinding>, String>> {
        let room = self.folder.review_room(task.build).await?;
        let prompt = review_build_prompt(&BuildReview {
            repo: &task.repo,
            issue: task.issue,
            title: &task.title,
            body: &task.body,
            tree: &checkout.room.tree,
            base,
            verify: &task.verify,
        });
        let env = env::nobody(&room);
        let step = format!("review-{round}");
        let run = self
            .run_agent(task, reviewer, &room, &prompt, &env, &step)
            .await?;
        let tree = &checkout.tree;
        if !Git.tree_clean(tree).await? || Git.tree_head(tree).await? != head {
            return Ok(Err(
                "The reviewer changed the build's worktree, which it may not, so the build \
                 pushed nothing."
                    .into(),
            ));
        }
        Ok(review_build_findings(&run.outcome, &run.text)
            .map_err(|e| format!("The reviewer did not review the change: {e:#}")))
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
            .run_agent(task, &self.agent, room, &prompt(None), env, step)
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
                &self.agent,
                room,
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
        agent: &LocalAgent,
        room: &Room,
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
        let run = agent
            .run(room, prompt, &self.access, env, &transcript, spawned)
            .await;
        // What the agent left running ends with its run.
        groups.stop(task.build).await?;
        let run = run?;
        say(task, &format!("the {step} run ended: {:?}", run.outcome));
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

/// How the local review went, for the PR.
pub(super) struct Reviewed {
    by: LocalAgent,
    /// The reviews read, the last of which found nothing.
    rounds: u32,
    /// The findings the builder fixed in the rounds before.
    fixed: usize,
}

/// The PR body's section on the local review, there whether or not one ran, so nobody reads
/// an unreviewed PR as reviewed.
fn reviewed_section(reviewed: Option<&Reviewed>) -> String {
    let Some(r) = reviewed else {
        return "### Local review\n\nThe local review was turned off on the machine that built \
                this PR (`[reviewer] enabled = false`), so no reviewer read the change before it \
                opened."
            .to_owned();
    };
    let model =
        r.by.model
            .as_deref()
            .map(|m| format!(", {m}"))
            .unwrap_or_default();
    let went = match (r.rounds, r.fixed) {
        (1, _) => "It found nothing to change.".to_owned(),
        (n, fixed) => format!(
            "It took {n} rounds: the builder fixed {fixed} finding(s), and the last round found \
             nothing."
        ),
    };
    format!(
        "### Local review\n\nBefore this PR opened, a fresh session of `{}` ({}{model}) read the \
         change against the issue, read-only, for bugs, parts of the issue left undone, risky \
         code without tests, and breaks of the repo's conventions. {went}",
        r.by.command,
        r.by.vendor.name()
    )
}

/// Why a build whose reviewer still asks for changes after the last round is stuck.
fn unreviewed(findings: &[BuildFinding]) -> String {
    let listed: String = findings
        .iter()
        .map(|f| format!("\n- `{}:{}`: {}", f.file, f.line, f.finding.trim()))
        .collect();
    format!(
        "The reviewer still asked for changes after {REVIEW_ROUNDS} rounds, so the build pushed \
         nothing:{listed}"
    )
}

/// Commits the change as the App's bot, with the developer as co-author, on `base`: the
/// tree's commit, or `None` when there is no change. Safe to repeat: each time it is one commit
/// on `base`.
async fn commit(
    task: &Task,
    checkout: &Checkout,
    base: &str,
    run: &Resolution,
) -> Result<Option<String>> {
    let (bot, dev) = (&task.author, &task.co_author);
    let message = format!(
        "{}\n\nCo-authored-by: {} <{}>\n",
        subject(task, run),
        dev.name,
        dev.email
    );
    let author = (bot.name.as_str(), bot.email.as_str());
    if !Git.squash(&checkout.tree, base, &message, author).await? {
        return Ok(None);
    }
    Ok(Some(Git.tree_head(&checkout.tree).await?))
}

fn subject(task: &Task, run: &Resolution) -> String {
    commit_subject(&run.text).unwrap_or_else(|| format!("Build #{}: {}", task.issue, task.title))
}

impl Builder {
    /// Pushes the committed change with a fresh token (the first may be near the end of its
    /// hour), and has the server open the PR.
    async fn publish(
        &self,
        lane: &Lane,
        task: &Task,
        checkout: &Checkout,
        run: &Resolution,
        reviewed: Option<&Reviewed>,
    ) -> Result<Work> {
        let head = Git.tree_head(&checkout.tree).await?;
        let token = self.mint(lane, task, Scope::Push).await?;
        if !push(task, checkout, &token, &head).await? {
            return Ok(Work::Stuck(format!(
                "The branch {} changed on GitHub while the build ran, so the build did not push \
             over it. Look at what is on it, then put the label on again to build anew.",
                task.branch
            )));
        }
        say(task, &format!("pushed {}", task.branch));
        let subject = subject(task, run);
        let description = format!(
            "{}\n\n{}",
            build_description(&run.text),
            reviewed_section(reviewed)
        )
        .trim()
        .to_owned();
        let pr = self.open(lane, task, &subject, &description).await?;
        say(task, &format!("opened PR #{pr}"));
        Ok(Work::Opened(pr))
    }

    /// Has the server open the build's PR, or finish labeling and assigning the one it has:
    /// called again while the server says to, or fails to answer, with a heartbeat between. The
    /// branch is pushed by now, so only a refusal ends the build.
    pub(super) async fn open(
        &self,
        lane: &Lane,
        task: &Task,
        title: &str,
        body: &str,
    ) -> Result<u64> {
        let said = |e: &anyhow::Error| say(task, &format!("{e:#}: calling again"));
        loop {
            match heard(lane, task, || lane.open_pr(task, title, body)).await {
                Ok(pr) => return Ok(pr),
                Err(e) => super::lane::again(self.pause, e, &said).await?,
            }
            self.beat(lane, task, task.stage()).await?;
        }
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

/// How the work ends a build whose PR is `pr`, when it has one: the one place that decides,
/// for a build and a landing alike.
fn settlement(work: Result<Work>, pr: Option<u64>) -> Settle {
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
        Some(Refusal::Closed) => {
            let why = closed(pr);
            Settle::Release(Some(why.clone()), Ended::Stuck(why))
        }
        _ => Settle::Release(Some(shorten(&said)), Ended::Stuck(said)),
    }
}

/// Why a build whose PR a person closed without merging is stuck: closing it said "not this".
fn closed(pr: Option<u64>) -> String {
    let pr = pr.map_or("the build's PR".to_owned(), |n| format!("PR #{n}"));
    format!("A person closed {pr} without merging it, so this build ends here.")
}

/// Tells the server how the work ended, and says how the build ended.
pub(super) async fn settle(lane: &Lane, task: &Task, work: Result<Work>) -> Result<Ended> {
    let (stuck, ended) = match settlement(work, task.pr) {
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

/// Heartbeats the build every `every`, saying the stage `stage` gives, until the server refuses
/// one: that refusal. One the server failed to answer for now is no refusal: the next tries again.
pub(super) async fn heartbeats(
    lane: &Lane,
    task: &Task,
    stage: impl Fn() -> Stage,
    every: Duration,
) -> anyhow::Error {
    loop {
        tokio::time::sleep(every).await;
        match lane.heartbeat(task, stage()).await {
            Ok(()) => {}
            Err(e) if unanswered(&e) => say(task, &format!("could not heartbeat: {e:#}")),
            Err(e) => return e,
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
            retry_after: None,
            text: "no".into(),
        }
        .into()
    }

    fn release(work: Result<Work>) -> Option<Option<String>> {
        match settlement(work, Some(9)) {
            Settle::Release(stuck, _) => Some(stuck),
            _ => None,
        }
    }

    #[test]
    fn each_ending_tells_the_server_what_it_must_hear() {
        assert!(matches!(
            settlement(Ok(Work::Opened(5)), None),
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
                settlement(Err(refused(409, Some(kind))), None),
                Settle::Nothing(Ended::Dropped(_))
            ));
        }
        assert!(matches!(
            settlement(Err(refused(409, Some(Refusal::Merged))), None),
            Settle::Merged
        ));
        assert_eq!(
            release(Err(refused(409, Some(Refusal::Closed)))),
            Some(Some(
                "A person closed PR #9 without merging it, so this build ends here.".into()
            ))
        );
        assert!(matches!(
            settlement(Err(refused(401, None)), None),
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
