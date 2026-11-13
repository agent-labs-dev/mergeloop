//! A built PR, from its opening until it merges: the daemon is its author in the land loop
//! (`next_need`), and asks for one need after another. A need for the author is worked by the
//! local agent in a fresh room on the PR's branch, as the App's bot: it changes files only, the
//! daemon commits and pushes its change, then posts its replies and resolves the threads it
//! names. A PR that waits, on the queue or on a human, holds no slot and has no room: all its
//! work is on GitHub, and the daemon keeps only its transcripts.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use mergepatrol_git::Git;
use mergepatrol_resolver::{
    ThreadAnswers, VerifyFailed, commit_subject, failed_note, thread_answers,
};
use serde_json::Value;
use tokio::sync::watch;

use super::build::{Builder, Ended, Stopped, Work, heard, heartbeats, push, say, settle, stopped};
use super::env;
use super::folder::Checkout;
use super::github::Bot;
use super::lane::{self, Lane, Scope, Stage, Standing, Task};
use crate::land::{self, Pr};

/// The agent runs one PR's needs may take before its build is released as stuck: a need that
/// comes back after each of the agent's runs would run it for good.
pub const ROUNDS: u32 = 10;

/// How many times the daemon posts a need's answer before the build is stuck: GitHub may refuse
/// for a moment.
const ANSWER_TRIES: u32 = 3;

/// What a need asks of the daemon.
#[derive(Debug, PartialEq, Eq)]
enum Act {
    /// The agent works it: review threads, or changes a required reviewer asked for.
    Work,
    /// Nothing for an agent: it waits on a human. A draft is one: the App opens its PRs ready,
    /// so a human made it a draft, and only they say when it is ready again.
    Wait,
    Ejected,
    Merged,
    /// The repo's queue does not run, or a need this daemon does not know: asked again later.
    Later,
}

fn act(needs: &str) -> Act {
    match needs {
        "threads" | "changes" => Act::Work,
        "draft" | "human" | "removed" => Act::Wait,
        "ejected" => Act::Ejected,
        "merged" => Act::Merged,
        _ => Act::Later,
    }
}

/// What the land loop does after a need.
enum Next {
    Ended(Work),
    /// Waits for a newer need.
    Seen,
    /// Asks for the need as it is now, at once.
    Again,
    /// Asks for the same need again, after a pause.
    Later,
}

/// What the land loop keeps between needs.
struct Landing {
    /// The need seen last.
    after: Option<String>,
    /// The need the server gave after the last run, which the loop goes on from.
    next: Option<Value>,
    ejected: u32,
    rounds: u32,
    /// What the server was told last: nothing yet, as a build taken up again may be either.
    standing: Option<Standing>,
    /// The need the agent's last run did not settle.
    unchanged: Option<String>,
    /// What of the answers to the need worked last went through.
    posted: Posted,
}

/// What of one need's answers went through, over every run on it: a try after a failure, or a
/// later run on the same need, posts only the rest.
#[derive(Default)]
struct Posted {
    need: String,
    replied: HashSet<String>,
    resolved: HashSet<String>,
    /// Threads whose reply failed on the way: GitHub may have taken it all the same.
    unsure: HashSet<String>,
}

impl Builder {
    /// Lands the PR of `task`, which has one, until it merges, it is stuck, the daemon no longer
    /// holds it, or `stop` turns true. A stopped daemon keeps the build, and lands it again when
    /// it starts. An error only when the sign-in is no good.
    pub async fn land(
        &self,
        lane: &Lane,
        task: &Task,
        stop: watch::Receiver<bool>,
    ) -> Result<Ended> {
        let work = tokio::select! {
            work = self.landing(lane, task) => work,
            e = heartbeats(lane, task, || task.stage(), self.heartbeat_every) => Err(e),
            () = stopped(stop) => Err(Stopped.into()),
        };
        let ended = match work {
            Err(e) if e.is::<Stopped>() => Ok(Ended::Dropped(
                "the daemon was stopped: it lands the PR again when it starts".into(),
            )),
            work => settle(lane, task, work).await,
        };
        self.clean_up(task).await;
        ended
    }

    async fn landing(&self, lane: &Lane, task: &Task) -> Result<Work> {
        let Some(number) = task.pr else {
            bail!("build {} has no PR to land", task.build);
        };
        say(task, &format!("landing PR #{number}"));
        let pr = Pr {
            repo: &task.repo,
            number,
            workspace: Some(lane.workspace),
            for_daemon: true,
        };
        let mut at = Landing {
            after: None,
            next: None,
            ejected: 0,
            rounds: 0,
            standing: None,
            unchanged: None,
            posted: Posted::default(),
        };
        loop {
            let Some(need) = self.ask(lane, task, &pr, &mut at).await? else {
                continue;
            };
            match self.on_need(lane, task, &pr, &need, &mut at).await? {
                Next::Ended(work) => return Ok(work),
                Next::Seen => at.after = need["id"].as_str().map(str::to_owned),
                Next::Again => at.after = None,
                Next::Later => tokio::time::sleep(self.pause).await,
            }
        }
    }

    /// The PR's next need, or `None` to ask again: after the server failed to answer, or, while
    /// the PR waits on a human, a heartbeat's time without a new need, which reports it waiting
    /// again, so a PR closed or merged meanwhile ends the build.
    async fn ask(
        &self,
        lane: &Lane,
        task: &Task,
        pr: &Pr<'_>,
        at: &mut Landing,
    ) -> Result<Option<Value>> {
        let waiting = at.standing == Some(Standing::Waiting);
        let asked = land::next(&lane.client, pr, &mut at.after, at.next.take());
        let asked = match waiting {
            true => match tokio::time::timeout(self.heartbeat_every, asked).await {
                Ok(asked) => asked,
                Err(_) => {
                    self.report(lane, task, Standing::Waiting).await?;
                    return Ok(None);
                }
            },
            false => asked.await,
        };
        match asked {
            Ok(need) => Ok(Some(need)),
            Err(e) => {
                let said = |e: &anyhow::Error| {
                    say(
                        task,
                        &format!("could not ask what PR #{} needs: {e:#}", pr.number),
                    );
                };
                lane::again(self.pause, e, said).await?;
                Ok(None)
            }
        }
    }

    /// Does what `need` asks of the daemon.
    async fn on_need(
        &self,
        lane: &Lane,
        task: &Task,
        pr: &Pr<'_>,
        need: &Value,
        at: &mut Landing,
    ) -> Result<Next> {
        let number = pr.number;
        let brief = need["brief"].as_str().unwrap_or_default();
        let stuck = |why: String| Ok(Next::Ended(Work::Stuck(why)));
        match act(need["needs"].as_str().unwrap_or_default()) {
            Act::Merged => return Ok(Next::Ended(Work::Merged)),
            Act::Ejected if at.ejected > 0 => {
                let reason = need["status"]["ejected"]["reason"].as_str();
                return stuck(format!(
                    "MergeLoop's queue ejected PR #{number} a second time: {}",
                    reason.unwrap_or(brief)
                ));
            }
            Act::Ejected => {
                at.ejected += 1;
                say(
                    task,
                    &format!("PR #{number} was ejected: it is queued again"),
                );
            }
            Act::Wait => {
                self.report(lane, task, Standing::Waiting).await?;
                at.standing = Some(Standing::Waiting);
                say(task, &format!("PR #{number} waits on a human: {brief}"));
            }
            Act::Work => return self.on_work(lane, task, pr, need, at).await,
            Act::Later => {
                say(task, &format!("PR #{number}: {brief}; asking again later"));
                return Ok(Next::Later);
            }
        }
        Ok(Next::Seen)
    }

    /// Has the agent work `need`, then asks whether that settled it.
    async fn on_work(
        &self,
        lane: &Lane,
        task: &Task,
        pr: &Pr<'_>,
        need: &Value,
        at: &mut Landing,
    ) -> Result<Next> {
        let number = pr.number;
        let brief = need["brief"].as_str().unwrap_or_default();
        let stuck = |why: String| Ok(Next::Ended(Work::Stuck(why)));
        let id = need["id"].as_str().unwrap_or_default();
        let unchanged = at.unchanged.as_deref() == Some(id);
        if at.rounds >= ROUNDS {
            let left = match unchanged {
                true => ", and the last left it as it was",
                false => "",
            };
            return stuck(format!(
                "PR #{number} still needs its author after {ROUNDS} runs of the agent{left}: {brief}"
            ));
        }
        at.rounds += 1;
        if at.standing != Some(Standing::Landing) {
            self.report(lane, task, Standing::Landing).await?;
            at.standing = Some(Standing::Landing);
        }
        if at.posted.need != id {
            at.posted = Posted {
                need: id.to_owned(),
                ..Posted::default()
            };
        }
        let run = (at.rounds, unchanged);
        let moved = match self
            .work_need(lane, task, number, need, run, &mut at.posted)
            .await?
        {
            Ok(moved) => moved,
            Err(why) => return stuck(why),
        };
        if moved {
            // A new head gives the PR a newer need once the server hears of it, which it may
            // not have yet: asked now, it could still show this one.
            at.unchanged = None;
            return Ok(Next::Seen);
        }
        match self.settled(lane, task, pr, id).await? {
            Some(next) => {
                at.unchanged = None;
                at.next = Some(next);
                Ok(Next::Seen)
            }
            None => {
                say(
                    task,
                    &format!("PR #{number} still needs it, as it was before the run"),
                );
                at.unchanged = Some(id.to_owned());
                Ok(Next::Again)
            }
        }
    }

    /// Whether a run that left the PR's head as it was settled need `id`, as the server says
    /// next: whatever the run posted, `None` when the PR's need is still `id`; else that need,
    /// for the loop to go on from.
    async fn settled(
        &self,
        lane: &Lane,
        task: &Task,
        pr: &Pr<'_>,
        id: &str,
    ) -> Result<Option<Value>> {
        let said = |e: &anyhow::Error| {
            say(
                task,
                &format!("could not ask what PR #{} needs now: {e:#}", pr.number),
            );
        };
        let need =
            lane::answered(self.pause, said, || land::ask(&lane.client, pr, Some(id))).await?;
        Ok((need["id"] != id).then_some(need))
    }

    /// Tells the server how the PR stands, which it refuses with how the PR ended when it is no
    /// longer open: closed by a person, or merged. Those end the build. A server that failed to
    /// answer, as when its read of GitHub failed, is told again after a pause.
    async fn report(&self, lane: &Lane, task: &Task, standing: Standing) -> Result<()> {
        let said = |e: &anyhow::Error| {
            say(task, &format!("could not say how its PR stands: {e:#}"));
        };
        lane::answered(self.pause, said, || lane.report(task, standing)).await
    }

    /// A token of `scope` for the build's repo, asked for again while the server fails to answer.
    pub(super) async fn mint(&self, lane: &Lane, task: &Task, scope: Scope) -> Result<String> {
        let said = |e: &anyhow::Error| say(task, &format!("could not mint a token: {e:#}"));
        lane::answered(self.pause, said, || {
            heard(lane, task, || lane.mint(task, scope))
        })
        .await
    }

    /// A heartbeat, sent again while the server fails to answer.
    async fn heartbeat(&self, lane: &Lane, task: &Task) -> Result<()> {
        self.beat(lane, task, task.stage()).await
    }

    /// A heartbeat saying `stage`, sent again while the server fails to answer.
    pub(super) async fn beat(&self, lane: &Lane, task: &Task, stage: Stage) -> Result<()> {
        let said = |e: &anyhow::Error| say(task, &format!("could not heartbeat: {e:#}"));
        lane::answered(self.pause, said, || lane.heartbeat(task, stage)).await
    }

    /// Works one need in a slot, in a room made for it and removed after. `run` is which run
    /// on the PR this is, and whether the last did not settle this need. `Err` with why the
    /// build is stuck, else whether the PR's head moved: this run pushed, or someone did.
    async fn work_need(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        need: &Value,
        run: (u32, bool),
        posted: &mut Posted,
    ) -> Result<Result<bool, String>> {
        let _slot = self.slots.clone().acquire_owned().await?;
        let worked = self.worked(lane, task, number, need, run, posted).await;
        self.clean_up(task).await;
        worked
    }

    async fn worked(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        need: &Value,
        (round, unchanged): (u32, bool),
        posted: &mut Posted,
    ) -> Result<Result<bool, String>> {
        self.heartbeat(lane, task).await?;
        let token = self.mint(lane, task, Scope::Push).await?;
        let checkout = self.folder.checkout_pr(task, &token, &self.access).await?;
        if let Some(in_room) = &self.in_room {
            in_room(task);
        }
        let base = Git.tree_head(&checkout.tree).await?;
        let needs = need["needs"].as_str().unwrap_or_default();
        say(
            task,
            &format!("PR #{number} needs {needs}: the agent works it"),
        );
        let mut brief = need["brief"].as_str().unwrap_or_default().to_owned();
        if unchanged {
            let open = self.open_threads(task, &token, number).await;
            brief = format!("{brief}\n\n{}", unsettled(&open));
        }
        let prompt = |failed: Option<&VerifyFailed>| match failed {
            None => brief.clone(),
            Some(f) => format!("{brief}\n\n{}", failed_note(f)),
        };
        let read = self.mint(lane, task, Scope::Read).await?;
        let env = env::bot(&checkout.room, &read, lane.client.server().url());
        let step = format!("need-{round}");
        let run = match self.change(task, &checkout, prompt, &env, &step).await? {
            Ok(run) => run,
            Err(why) => return Ok(Err(why)),
        };
        // No local review: MergeLoop's own review reads what this pushes to the PR.
        let Some(pushed) = self
            .publish_need(lane, task, &checkout, &base, &run.text, number)
            .await?
        else {
            // Someone pushed to the PR while the agent worked: the next need says what their
            // push left to do.
            say(
                task,
                &format!(
                    "{} changed on GitHub while the agent worked: its change is dropped",
                    task.branch
                ),
            );
            return Ok(Ok(true));
        };
        let answered = self
            .answer(lane, task, number, &run.text, pushed.as_deref(), posted)
            .await?;
        Ok(answered.map(|()| pushed.is_some()))
    }

    /// The ids of the PR's open review threads, or none when GitHub would not say.
    async fn open_threads(&self, task: &Task, token: &str, number: u64) -> Vec<String> {
        let bot = Bot {
            http: &self.http,
            api: &self.github,
            token,
        };
        match bot.threads(&task.repo, number).await {
            Ok(threads) => threads
                .into_iter()
                .filter(|t| !t.resolved)
                .map(|t| t.id)
                .collect(),
            Err(e) => {
                say(
                    task,
                    &format!("could not read PR #{number}'s threads: {e:#}"),
                );
                vec![]
            }
        }
    }

    /// Commits the agent's change on the PR's head as the App's bot, with the developer as
    /// co-author, and pushes it: the head pushed, or `Some(None)` when the agent changed nothing.
    /// `None` when the branch moved on GitHub since the checkout.
    async fn publish_need(
        &self,
        lane: &Lane,
        task: &Task,
        checkout: &Checkout,
        base: &str,
        text: &str,
        number: u64,
    ) -> Result<Option<Option<String>>> {
        let subject = commit_subject(text)
            .unwrap_or_else(|| format!("Work what PR #{number} needs of its author"));
        let (bot, dev) = (&task.author, &task.co_author);
        let message = format!(
            "{subject}\n\nCo-authored-by: {} <{}>\n",
            dev.name, dev.email
        );
        let author = (bot.name.as_str(), bot.email.as_str());
        if !Git.squash(&checkout.tree, base, &message, author).await? {
            return Ok(Some(None));
        }
        let head = Git.tree_head(&checkout.tree).await?;
        let token = self.mint(lane, task, Scope::Push).await?;
        if !push(task, checkout, &token, &head).await? {
            return Ok(None);
        }
        say(task, &format!("pushed {}", task.branch));
        Ok(Some(Some(head)))
    }

    /// Posts the agent's replies on the PR's threads as the App's bot, naming the commit pushed,
    /// then resolves the threads it named: only the PR's own threads, and only what `posted`
    /// does not hold yet. A failure is tried again, heartbeating. `Err` with what GitHub refused
    /// when it still did after `ANSWER_TRIES`.
    async fn answer(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        text: &str,
        pushed: Option<&str>,
        posted: &mut Posted,
    ) -> Result<Result<(), String>> {
        let answers = thread_answers(text);
        if answers.replies.is_empty() && answers.resolve.is_empty() {
            return Ok(Ok(()));
        }
        for tried in 1.. {
            let token = self.mint(lane, task, Scope::Push).await?;
            let bot = Bot {
                http: &self.http,
                api: &self.github,
                token: &token,
            };
            let failed = match self
                .post(&bot, task, number, &answers, pushed, posted)
                .await
            {
                Ok(()) => break,
                Err(e) => e,
            };
            if tried >= ANSWER_TRIES {
                return Ok(Err(format!(
                    "GitHub refused the daemon's answer on PR #{number} {ANSWER_TRIES} times: {failed:#}"
                )));
            }
            say(task, &format!("{failed:#}: trying again"));
            tokio::time::sleep(self.pause).await;
            self.heartbeat(lane, task).await?;
        }
        Ok(Ok(()))
    }

    /// One try at posting what of `answers` is not in `posted` yet, noting there what went
    /// through. A reply that failed on an earlier try counts as posted when the bot's newest
    /// comment in its thread is that reply: GitHub took it, and its answer was lost.
    async fn post(
        &self,
        bot: &Bot<'_>,
        task: &Task,
        number: u64,
        answers: &ThreadAnswers,
        pushed: Option<&str>,
        posted: &mut Posted,
    ) -> Result<()> {
        let threads = bot
            .threads(&task.repo, number)
            .await
            .with_context(|| format!("could not read PR #{number}'s threads"))?;
        let thread_of = |id: &str| threads.iter().find(|t| t.id == id);
        for (thread, reply) in &answers.replies {
            if posted.replied.contains(thread) {
                continue;
            }
            let Some(there) = thread_of(thread) else {
                say(
                    task,
                    &format!("PR #{number} has no thread {thread}: no reply"),
                );
                continue;
            };
            let body = match pushed {
                Some(sha) => format!("{reply}\n\nPushed in {}.", &sha[..sha.len().min(7)]),
                None => reply.clone(),
            };
            let took = posted.unsure.contains(thread) && there.bot_said.as_deref() == Some(&body);
            if !took {
                posted.unsure.insert(thread.clone());
                bot.reply(thread, &body)
                    .await
                    .with_context(|| format!("could not reply in {thread}"))?;
            }
            posted.unsure.remove(thread);
            posted.replied.insert(thread.clone());
        }
        for thread in &answers.resolve {
            let open = thread_of(thread).is_some_and(|t| !t.resolved);
            if posted.resolved.contains(thread) || !open {
                continue;
            }
            bot.resolve(thread)
                .await
                .with_context(|| format!("could not resolve {thread}"))?;
            posted.resolved.insert(thread.clone());
        }
        Ok(())
    }
}

/// What the next run on a need is told when the last did not settle it: the threads still open.
fn unsettled(open: &[String]) -> String {
    let open = match open {
        [] => String::new(),
        ids => format!(" Review threads still open: {}.", ids.join(", ")),
    };
    format!(
        "MergeLoop still shows this need as it was before your last run, so that run did not settle it.{open} A reply alone does not close a thread: it stays open until it is resolved. The replies you gave are posted already, and are not posted again."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_review_threads_and_requested_changes_run_the_agent() {
        assert_eq!(act("threads"), Act::Work);
        assert_eq!(act("changes"), Act::Work);
        for waits in ["draft", "human", "removed"] {
            assert_eq!(act(waits), Act::Wait, "{waits}");
        }
        assert_eq!(act("ejected"), Act::Ejected);
        assert_eq!(act("merged"), Act::Merged);
        assert_eq!(act("off"), Act::Later);
        assert_eq!(act("something new"), Act::Later);
    }
}
