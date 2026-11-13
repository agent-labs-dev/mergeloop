//! A built PR, from its opening until it merges: the daemon is its author in the land loop
//! (`next_need`), and asks for one need after another. A need for the author is worked by the
//! local agent in a fresh room on the PR's branch, as the App's bot: it changes files only, the
//! daemon commits and pushes its change, then posts its replies and resolves the threads it
//! names. A PR that waits, on the queue or on a human, holds no slot and has no room: all its
//! work is on GitHub, and the daemon keeps only its transcripts.

use anyhow::{Result, bail};
use mergepatrol_git::Git;
use mergepatrol_resolver::{VerifyFailed, commit_subject, failed_note, thread_answers};
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
            Act::Work if at.rounds >= ROUNDS => {
                return stuck(format!(
                    "PR #{number} still needs its author after {ROUNDS} runs of the agent: {brief}"
                ));
            }
            Act::Work => {
                at.rounds += 1;
                self.stand(lane, task, at, Standing::Landing).await?;
                if let Some(why) = self.work_need(lane, task, number, need, at.rounds).await? {
                    return stuck(why);
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

    /// Works one need in a slot, in a room made for it and removed after. `Some` with why the
    /// build is stuck.
    async fn work_need(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        need: &Value,
        round: u32,
    ) -> Result<Option<String>> {
        let _slot = self.slots.clone().acquire_owned().await?;
        let worked = self.worked(lane, task, number, need, round).await;
        self.clean_up(task).await;
        worked
    }

    async fn worked(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        need: &Value,
        round: u32,
    ) -> Result<Option<String>> {
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
        let prompt = |failed: Option<&VerifyFailed>| match failed {
            None => brief.to_owned(),
            Some(f) => format!("{brief}\n\n{}", failed_note(f)),
        };
        let read = heard(lane, task, || lane.mint(task, Scope::Read)).await?;
        let env = env::bot(&checkout.room, &read, lane.client.server().url());
        let step = format!("need-{round}");
        let run = match self.change(task, &checkout, prompt, &env, &step).await? {
            Ok(run) => run,
            Err(why) => return Ok(Some(why)),
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
            return Ok(None);
        };
        self.answer(lane, task, number, &run.text, pushed.as_deref())
            .await?;
        Ok(None)
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

    /// Posts the agent's replies on the PR's threads as the App's bot, with a fresh token,
    /// naming the commit pushed, then resolves the threads it named. Only the PR's own threads; what fails is said and
    /// skipped: the next need says what is left.
    async fn answer(
        &self,
        lane: &Lane,
        task: &Task,
        number: u64,
        text: &str,
        pushed: Option<&str>,
    ) -> Result<()> {
        let answers = thread_answers(text);
        if answers.replies.is_empty() && answers.resolve.is_empty() {
            return Ok(());
        }
        let token = heard(lane, task, || lane.mint(task, Scope::Push)).await?;
        let bot = Bot {
            http: &self.http,
            api: &self.github,
            token: &token,
        };
        let threads = match bot.threads(&task.repo, number).await {
            Ok(threads) => threads,
            Err(e) => {
                say(
                    task,
                    &format!("could not read PR #{number}'s threads: {e:#}"),
                );
                return Ok(());
            }
        };
        let open = |thread: &str| threads.iter().find(|(id, _)| id == thread).map(|t| !t.1);
        for (thread, reply) in &answers.replies {
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
            if let Err(e) = bot.reply(thread, &body).await {
                say(task, &format!("could not reply in {thread}: {e:#}"));
            }
        }
        for thread in answers.resolve.iter().filter(|t| open(t) == Some(true)) {
            if let Err(e) = bot.resolve(thread).await {
                say(task, &format!("could not resolve {thread}: {e:#}"));
            }
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
