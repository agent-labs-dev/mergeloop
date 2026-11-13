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
use super::lane::{Lane, Scope, Standing, Task};
use crate::client::{needs_sign_in, refusal};
use crate::land::{self, Pr};

/// The agent runs one PR's needs may take before its build is released as stuck: a need that
/// comes back after each of the agent's runs would run it for good.
pub const ROUNDS: u32 = 10;

/// How many times the daemon posts a need's answer before the build is stuck: GitHub may refuse
/// for a moment.
const ANSWER_TRIES: u32 = 3;

/// What the next run on a need is told when the last one left it as it was.
const UNCHANGED: &str = "Your last run on this need changed no file and answered no thread, so the need is as it was. Do what it asks this time, or answer it as this brief says.";

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
    ejected: u32,
    rounds: u32,
    /// What the server was told last: nothing yet, as a build taken up again may be either.
    standing: Option<Standing>,
    /// The need the agent's last run left as it was.
    unchanged: Option<String>,
}

/// What working a need came to.
enum Worked {
    /// Something went to GitHub: a push or an answer, or someone else's push. The next need
    /// says what is left.
    Changed,
    /// Nothing did: the need is as it was.
    Unchanged,
    Stuck(String),
}

/// What of a need's answer went through: a try after a refusal posts only the rest.
#[derive(Default)]
struct Posted {
    replied: HashSet<String>,
    resolved: HashSet<String>,
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
            e = heartbeats(lane, task, self.heartbeat_every) => Err(e),
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
            ejected: 0,
            rounds: 0,
            standing: None,
            unchanged: None,
        };
        loop {
            let need = match land::next(&lane.client, &pr, &mut at.after).await {
                Ok(need) => need,
                Err(e) if needs_sign_in(&e) || refusal(&e).is_some() => return Err(e),
                Err(e) => {
                    say(
                        task,
                        &format!("could not ask what PR #{number} needs: {e:#}"),
                    );
                    tokio::time::sleep(self.pause).await;
                    continue;
                }
            };
            match self.on_need(lane, task, number, &need, &mut at).await? {
                Next::Ended(work) => return Ok(work),
                Next::Seen => at.after = need["id"].as_str().map(str::to_owned),
                Next::Again => at.after = None,
                Next::Later => tokio::time::sleep(self.pause).await,
            }
        }
    }

    /// Does what `need` asks of the daemon.
    async fn on_need(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        need: &Value,
        at: &mut Landing,
    ) -> Result<Next> {
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
                self.stand(lane, task, at, Standing::Waiting).await?;
                say(task, &format!("PR #{number} waits on a human: {brief}"));
            }
            Act::Work => {
                let id = need["id"].as_str();
                let unchanged = id.is_some() && at.unchanged.as_deref() == id;
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
                self.stand(lane, task, at, Standing::Landing).await?;
                let run = (at.rounds, unchanged);
                match self.work_need(lane, task, number, need, run).await? {
                    Worked::Stuck(why) => return stuck(why),
                    Worked::Changed => at.unchanged = None,
                    Worked::Unchanged => {
                        say(
                            task,
                            &format!("PR #{number}: the agent's run left it as it was"),
                        );
                        at.unchanged = id.map(str::to_owned);
                        return Ok(Next::Again);
                    }
                }
            }
            Act::Later => {
                say(task, &format!("PR #{number}: {brief}; asking again later"));
                return Ok(Next::Later);
            }
        }
        Ok(Next::Seen)
    }

    /// Tells the server how the PR stands, when that changed.
    async fn stand(
        &self,
        lane: &Lane,
        task: &Task,
        at: &mut Landing,
        standing: Standing,
    ) -> Result<()> {
        if at.standing != Some(standing) {
            lane.report(task, standing).await?;
            at.standing = Some(standing);
        }
        Ok(())
    }

    /// Works one need in a slot, in a room made for it and removed after. `run` is which run
    /// on the PR this is, and whether the last left this need as it was.
    async fn work_need(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        need: &Value,
        run: (u32, bool),
    ) -> Result<Worked> {
        let _slot = self.slots.clone().acquire_owned().await?;
        let worked = self.worked(lane, task, number, need, run).await;
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
    ) -> Result<Worked> {
        lane.heartbeat(task, task.stage()).await?;
        let token = heard(lane, task, || lane.mint(task, Scope::Push)).await?;
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
        let brief = need["brief"].as_str().unwrap_or_default();
        let prompt = |failed: Option<&VerifyFailed>| {
            let mut prompt = brief.to_owned();
            if unchanged {
                prompt = format!("{prompt}\n\n{UNCHANGED}");
            }
            match failed {
                None => prompt,
                Some(f) => format!("{prompt}\n\n{}", failed_note(f)),
            }
        };
        let read = heard(lane, task, || lane.mint(task, Scope::Read)).await?;
        let env = env::bot(&checkout.room, &read, lane.client.server().url());
        let step = format!("need-{round}");
        let run = match self.change(task, &checkout, prompt, &env, &step).await? {
            Ok(run) => run,
            Err(why) => return Ok(Worked::Stuck(why)),
        };
        // Ticket 07's review of the change goes here, before anything is pushed.
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
            return Ok(Worked::Changed);
        };
        Ok(
            match self
                .answer(lane, task, number, &run.text, pushed.as_deref())
                .await?
            {
                Err(why) => Worked::Stuck(why),
                Ok(answered) if answered || pushed.is_some() => Worked::Changed,
                Ok(_) => Worked::Unchanged,
            },
        )
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
        let token = heard(lane, task, || lane.mint(task, Scope::Push)).await?;
        if !push(task, checkout, &token, &head).await? {
            return Ok(None);
        }
        say(task, &format!("pushed {}", task.branch));
        Ok(Some(Some(head)))
    }

    /// Posts the agent's replies on the PR's threads as the App's bot, naming the commit pushed,
    /// then resolves the threads it named: only the PR's own threads. A refusal is tried again,
    /// heartbeating, without posting twice what went through. `Ok(true)` when anything was
    /// posted; `Err` with what GitHub refused when it still did after `ANSWER_TRIES`.
    async fn answer(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        text: &str,
        pushed: Option<&str>,
    ) -> Result<Result<bool, String>> {
        let answers = thread_answers(text);
        if answers.replies.is_empty() && answers.resolve.is_empty() {
            return Ok(Ok(false));
        }
        let mut posted = Posted::default();
        for tried in 1.. {
            let token = heard(lane, task, || lane.mint(task, Scope::Push)).await?;
            let bot = Bot {
                http: &self.http,
                api: &self.github,
                token: &token,
            };
            let refused = match self
                .post(&bot, task, number, &answers, pushed, &mut posted)
                .await
            {
                Ok(()) => break,
                Err(e) => e,
            };
            if tried >= ANSWER_TRIES {
                return Ok(Err(format!(
                    "GitHub refused the daemon's answer on PR #{number} {ANSWER_TRIES} times: {refused:#}"
                )));
            }
            say(task, &format!("{refused:#}: trying again"));
            tokio::time::sleep(self.pause).await;
            lane.heartbeat(task, task.stage()).await?;
        }
        Ok(Ok(!posted.replied.is_empty() || !posted.resolved.is_empty()))
    }

    /// One try at posting what of `answers` is not in `posted` yet, noting there what went
    /// through.
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
        let open = |thread: &str| threads.iter().find(|(id, _)| id == thread).map(|t| !t.1);
        for (thread, reply) in &answers.replies {
            if posted.replied.contains(thread) {
                continue;
            }
            if open(thread).is_none() {
                say(
                    task,
                    &format!("PR #{number} has no thread {thread}: no reply"),
                );
                continue;
            }
            let body = match pushed {
                Some(sha) => format!("{reply}\n\nPushed in {}.", &sha[..sha.len().min(7)]),
                None => reply.clone(),
            };
            bot.reply(thread, &body)
                .await
                .with_context(|| format!("could not reply in {thread}"))?;
            posted.replied.insert(thread.clone());
        }
        for thread in &answers.resolve {
            if posted.resolved.contains(thread) || open(thread) != Some(true) {
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
