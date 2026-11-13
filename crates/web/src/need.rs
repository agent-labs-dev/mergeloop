//! What a queued PR needs next from the agent that queued it, for the `mergeloop` CLI and the
//! `next_need` tool: one need at a time, read from where the queue has the PR, with a brief that
//! says what to do and the exact command to run after. The agent keeps no state: what it last
//! saw is the need's `id`, which it passes back as `after`, and the route waits until there is
//! something newer.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use mergepatrol_core::RepoName;
use mergepatrol_resolver::{COMMIT, REPLY, RESOLVE};
use mergepatrol_store::{EventRecord, ExitRecord};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use ts_rs::TS;

use crate::api::repo_name;
use crate::auth::{Caller, hash};
use crate::v1::{HoldReasonView, PullStateView, QueueStateView, RepoStateView};
use crate::{ApiError, AppState};

/// The longest a call waits for something new. The load balancer allows an hour.
const MAX_WAIT: u64 = 50;
/// How often a waiting call reads the PR again.
const POLL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum NeedKind {
    /// It is not queued: queue it.
    Queue,
    /// Nothing for the agent yet: the queue is on it.
    Wait,
    /// Review threads on it are unresolved: work them.
    Threads,
    /// A reviewer whose approval is required asked for changes: make them.
    Changes,
    /// It is a draft: publish it once its work is done.
    Draft,
    /// It waits on a human: nothing for the agent to do.
    Human,
    /// The queue ejected it: fix the cause, then queue it again.
    Ejected,
    /// Someone took it out of the queue.
    Removed,
    Merged,
    /// The repo's queue does not run.
    Off,
}

impl NeedKind {
    /// The `mergeloop` CLI's exit code for it: 0 for work to do, 10 merged, 11 ejected, 12
    /// waiting on a human, 2 for a queue that cannot take it.
    fn exit_code(self) -> u8 {
        match self {
            NeedKind::Queue
            | NeedKind::Wait
            | NeedKind::Threads
            | NeedKind::Changes
            | NeedKind::Draft => 0,
            NeedKind::Merged => 10,
            NeedKind::Ejected => 11,
            NeedKind::Human | NeedKind::Removed => 12,
            NeedKind::Off => 2,
        }
    }
}

/// What a PR needs next.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct NeedView {
    pub needs: NeedKind,
    /// This need, as it is now. Pass it back as `after` to wait for the next one.
    pub id: String,
    /// `owner/name`.
    pub repo: String,
    pub number: u32,
    /// The stack's layer the need is on, when the PR is in a stack.
    pub layer: Option<u32>,
    /// What to do, for the agent.
    pub brief: String,
    /// The command to run once it is done.
    pub then: String,
    /// The `mergeloop` CLI's exit code for it.
    pub exit_code: u8,
    /// Where the queue has the PR, as `pull_status` says it.
    pub status: PullStateView,
}

#[derive(Deserialize)]
pub(crate) struct NextQuery {
    after: Option<String>,
    wait: Option<u64>,
    /// The brief is for an agent that `mergeloop daemon` runs on a PR it built.
    for_daemon: Option<bool>,
}

/// Who reads a need's brief: what it may do on GitHub differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reader {
    /// An agent that commits, pushes, and acts on GitHub itself.
    Agent,
    /// An agent that `mergeloop daemon` runs on a PR it built, as the App's bot: it changes
    /// files only, and the daemon commits, pushes, replies and resolves for it once it ends.
    Daemon,
}

pub(crate) async fn next_need(
    State(app): State<AppState>,
    caller: Caller,
    Path((id, owner, name, number)): Path<(u64, String, String, u64)>,
    Query(q): Query<NextQuery>,
) -> Result<Json<NeedView>, ApiError> {
    let repo = repo_name(owner, name)?;
    let id = caller.repo(&app, id, &repo).await?;
    let after = q.after.as_deref();
    let reader = match q.for_daemon {
        Some(true) => Reader::Daemon,
        _ => Reader::Agent,
    };
    let deadline = Instant::now() + Duration::from_secs(q.wait.unwrap_or(0).min(MAX_WAIT));
    loop {
        let view = read(&app, id, &repo, number, after, reader).await?;
        let seen = Some(view.id.as_str()) == after;
        let settled = matches!(view.needs, NeedKind::Merged | NeedKind::Off);
        if settled || (!seen && view.needs != NeedKind::Wait) {
            return Ok(Json(view));
        }
        if Instant::now() + POLL > deadline {
            // Nothing newer than what the agent saw: it waits on.
            return Ok(Json(if seen { waiting(view) } else { view }));
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn read(
    app: &AppState,
    id: u64,
    repo: &RepoName,
    number: u64,
    after: Option<&str>,
    reader: Reader,
) -> Result<NeedView, ApiError> {
    let status = crate::v1::statuses(app, repo, Some(id), &[number])
        .await?
        .remove(0);
    let exits = app.store.last_exits(id, repo, &[number]).await?;
    let exit = exits.iter().find(|e| e.pr == number);
    let newest = app.store.events(id, repo, Some(number), 1).await?;
    Ok(view(status, exit, newest.first(), after, reader))
}

/// The need of a PR, from where the queue has it, how it last left the queue, and its newest
/// event. `after` is the need the agent saw last.
fn view(
    status: PullStateView,
    exit: Option<&ExitRecord>,
    newest: Option<&EventRecord>,
    after: Option<&str>,
    reader: Reader,
) -> NeedView {
    let exit = exit.filter(|_| status.queue.is_none());
    let mut need = need(&status, exit);
    let mut id = need_id(&need, exit, newest);
    // The agent saw it ejected, and runs `then`: it queues it again, and waits on that
    // until the queue takes it.
    let requeue = format!("{id}-q");
    // Not one someone took out: only they, or the user, queue it again.
    if need.kind == NeedKind::Ejected
        && (after == Some(id.as_str()) || after == Some(requeue.as_str()))
    {
        need = Need {
            kind: NeedKind::Queue,
            layer: None,
            detail: need.detail,
        };
        id = requeue;
    }
    let number = status.number;
    let then = format!("mergeloop {}#{number} --after {id}", status.repo);
    NeedView {
        brief: match reader {
            Reader::Agent => brief(&need, &status, &then),
            Reader::Daemon => {
                daemon_brief(&need, &status).unwrap_or_else(|| brief(&need, &status, &then))
            }
        },
        needs: need.kind,
        exit_code: need.kind.exit_code(),
        id,
        repo: status.repo.clone(),
        number,
        layer: need.layer,
        then,
        status,
    }
}

/// A need the agent already saw, while it waits for the next.
fn waiting(mut view: NeedView) -> NeedView {
    view.needs = NeedKind::Wait;
    view.exit_code = NeedKind::Wait.exit_code();
    view.brief = format!(
        "Nothing new since `{}`. Run `{}` again.",
        view.id, view.then
    );
    view
}

#[derive(Debug, PartialEq)]
struct Need {
    kind: NeedKind,
    layer: Option<u32>,
    /// What the queue says of it: the hold, or how it left.
    detail: String,
}

fn need(status: &PullStateView, exit: Option<&ExitRecord>) -> Need {
    let of = |kind, detail: String| Need {
        kind,
        layer: None,
        detail,
    };
    if status.mergeloop.state != RepoStateView::On {
        let why = status.mergeloop.config_error.clone().map_or_else(
            || "MergeLoop is not installed on it, it has no config file, or the dashboard turned it off".to_owned(),
            |e| format!("its config file does not parse: {e}"),
        );
        return of(NeedKind::Off, why);
    }
    let Some(queue) = &status.queue else {
        return match exit {
            Some(e) if e.outcome == "merged" => of(NeedKind::Merged, e.detail.clone()),
            Some(e) if e.outcome == "ejected" => of(NeedKind::Ejected, e.detail.clone()),
            Some(e) => of(NeedKind::Removed, e.outcome.clone()),
            None => of(NeedKind::Queue, "it is not queued".into()),
        };
    };
    let hold = match (queue.state, &queue.hold) {
        (QueueStateView::Held, Some(hold)) => hold,
        (QueueStateView::Running, _) => {
            return of(NeedKind::Wait, "the queue is running it".into());
        }
        _ => {
            return of(
                NeedKind::Wait,
                format!("it is number {} in line", queue.position),
            );
        }
    };
    let kind = match hold.reason {
        HoldReasonView::ReviewChanges | HoldReasonView::UnresolvedThreads => NeedKind::Threads,
        HoldReasonView::ChangesRequested => NeedKind::Changes,
        HoldReasonView::Draft => NeedKind::Draft,
        HoldReasonView::ReviewAsks
        | HoldReasonView::ReviewRequired
        | HoldReasonView::WorkflowApproval
        | HoldReasonView::ForkEditsOff
        | HoldReasonView::NoQueue
        | HoldReasonView::NoSubscription => NeedKind::Human,
        // Each lifts by itself.
        HoldReasonView::FairUseLimit
        | HoldReasonView::PrepRebasing
        | HoldReasonView::PrepChecks
        | HoldReasonView::PrepFixing
        | HoldReasonView::Reviewing => NeedKind::Wait,
    };
    Need {
        kind,
        layer: hold.layer,
        detail: hold.message.clone(),
    }
}

/// The need as it is now: a new event or a new hold makes it a new one, even of the same kind,
/// as a review that sends findings back again after a push.
fn need_id(need: &Need, exit: Option<&ExitRecord>, newest: Option<&EventRecord>) -> String {
    let newest = newest.map(|e| format!("{}|{}|{}", e.at, e.kind, e.message));
    let key = format!(
        "{:?}|{:?}|{}|{:?}|{newest:?}",
        need.kind,
        need.layer,
        need.detail,
        exit.map(|e| e.id),
    );
    hash(&key)[..16].to_owned()
}

/// What the agent does about the need.
fn brief(need: &Need, status: &PullStateView, then: &str) -> String {
    let (repo, number) = (status.repo.as_str(), status.number);
    let pr = need.layer.unwrap_or(number);
    let on = match need.layer {
        Some(layer) if layer != number => format!(" on #{layer}, a layer of its stack,"),
        _ => String::new(),
    };
    let detail = &need.detail;
    let args = args(repo, pr);
    match need.kind {
        NeedKind::Queue => format!(
            "#{number} is not queued. Queue it with `mergeloop call queue_pull {args}`, then run `{then}`."
        ),
        NeedKind::Wait => format!("Nothing for you yet: {detail}. Run `{then}`."),
        NeedKind::Threads => THREADS
            .replace("{on}", &on)
            .replace("{detail}", detail)
            .replace("{threads}", &threads(repo, pr))
            .replace("{args}", &args)
            .replace("{then}", then),
        NeedKind::Changes => format!(
            "A reviewer whose approval the branch requires asked for changes{on}. Read their review (`gh pr view {pr} --repo {repo} --comments`), make the changes, fetch and rebase onto the remote branch, push once, and reply to each of their comments with what changed. The PR goes on once they approve. Then run `{then}`."
        ),
        NeedKind::Draft => format!(
            "#{pr} is a draft{on}. If its work is done, publish it with `gh pr ready {pr} --repo {repo}`. If not, finish it first, or tell the user it waits on them. Then run `{then}`."
        ),
        NeedKind::Human => format!(
            "It waits on a human: {detail}. Nothing here is yours to do. Tell the user what it waits for, then run `{then}` in the background: it returns once something changes."
        ),
        NeedKind::Ejected => format!(
            "MergeLoop ejected #{number}: {detail}. Fix the cause on its branch, then fetch, rebase onto the remote branch, and push. Then run `{then}`, which queues it again."
        ),
        NeedKind::Removed => format!(
            "#{number} was taken out of the queue ({detail}), not by a failure. Don't queue it again unless the user asks you to: if they do, run `mergeloop call queue_pull {args}`. Tell the user, then run `{then}` in the background: it returns once someone queues it again."
        ),
        NeedKind::Merged => format!("#{number} merged, as {detail}. Nothing is left to do."),
        NeedKind::Off => format!(
            "MergeLoop does not queue {repo}: {detail}. A human sets it up, or turns it on, in the MergeLoop dashboard. Tell the user."
        ),
    }
}

/// The PR's `mergeloop call` arguments.
fn args(repo: &str, pr: u32) -> String {
    format!(r#"'{{"repo":"{repo}","number":{pr}}}'"#)
}

/// The command that lists the PR's review threads.
fn threads(repo: &str, pr: u32) -> String {
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    format!(
        r#"`gh api graphql -f query='{{ repository(owner: "{owner}", name: "{name}") {{ pullRequest(number: {pr}) {{ reviewThreads(first: 100) {{ nodes {{ id isResolved path line comments(first: 20) {{ nodes {{ author {{ login }} body }} }} }} }} }} }} }}'`"#
    )
}

/// What the agent the daemon runs does about the need, when it is one an agent works.
fn daemon_brief(need: &Need, status: &PullStateView) -> Option<String> {
    let (repo, number) = (status.repo.as_str(), status.number);
    let pr = need.layer.unwrap_or(number);
    let on = match need.layer {
        Some(layer) if layer != number => format!(" on #{layer}, a layer of its stack,"),
        _ => String::new(),
    };
    let fill = |text: &str| {
        text.replace("{on}", &on)
            .replace("{detail}", &need.detail)
            .replace("{repo}", repo)
            .replace("{pr}", &pr.to_string())
            .replace("{threads}", &threads(repo, pr))
            .replace("{args}", &args(repo, pr))
            .replace("{after}", DAEMON_AFTER)
            .replace("{REPLY}", REPLY)
            .replace("{RESOLVE}", RESOLVE)
            .replace("{COMMIT}", COMMIT)
    };
    match need.kind {
        NeedKind::Threads => Some(fill(DAEMON_THREADS)),
        NeedKind::Changes => Some(fill(DAEMON_CHANGES)),
        _ => None,
    }
}

/// What the daemon does once its agent ends.
const DAEMON_AFTER: &str = "You work in a checkout of the PR's branch, and `gh` acts as MergeLoop's App, which opened the PR. Change files only: do not commit, push, reply or resolve. Once you end, MergeLoop commits your change as one commit, pushes it, then posts your replies, each naming that commit, and resolves the threads you name.";

const DAEMON_THREADS: &str = "Review threads{on} hold the PR: {detail}. Work each unresolved one now. {after}

1. List the unresolved review threads of #{pr}: {threads}. MergeLoop's own findings, with their ids and evidence: `mergeloop call get_agent_review {args}`.
2. Read each thread and the code it points at. Fix it, at every place its **Also at** names, or decide it doesn't apply, for a reason the code shows: a file and line, a test, a doc. Only a call on scope, product or design, access you lack, or a change outside the repo is for a human.
3. End your last message with a line `{REPLY} <thread id> <reply>` for each thread you worked: one line on what changed, or the reason it doesn't apply. Then a line `{RESOLVE} <thread id>` for each you fixed or that doesn't apply. Another reviewer's thread that needs a human: reply with the decision needed and who should make it, and do not resolve it. If you changed files, end with a line `{COMMIT} <subject>`, the commit's one-line subject.

MergeLoop's reviewer reads no reply from the App, so a finding of its that doesn't apply is settled by resolving its thread, with the reason as your reply. Leave each thread MergeLoop's review marked **For a human** to them, with no reply: resolving it answers it as fine as it is. Never approve the PR.";

const DAEMON_CHANGES: &str = "A reviewer whose approval the branch requires asked for changes{on}. {after}

Read their review, `gh pr view {pr} --repo {repo} --json reviews`, and their comments on its lines in the review threads: {threads}. Make the changes. End your last message with a line `{REPLY} <thread id> <reply>` for each of their threads, one line on what changed, and a line `{COMMIT} <subject>`, the commit's one-line subject. The PR goes on once they approve.";

const THREADS: &str = "Review threads{on} hold the PR: {detail}. Work each unresolved one now, without waiting to be asked:

1. List the unresolved review threads of #{pr}: {threads}. MergeLoop's own findings, with their ids and evidence: `mergeloop call get_agent_review {args}`.
2. Read each thread and the code it points at. Fix it, at every place its **Also at** names, or decide it doesn't apply, for a reason the code shows: a file and line, a test, a doc. Only a call on scope, product or design, access you lack, or a change outside the repo is for a human.
3. Commit the fixes together, as few commits as you can: each push gets a new review. Fetch and rebase onto the remote branch, then push once.
4. Reply in each thread you worked: the fix's short SHA and one line on what changed, or the reason it doesn't apply. Then resolve it: `gh api graphql -f query='mutation { resolveReviewThread(input: {threadId: \"ID\"}) { thread { isResolved } } }'`. Resolve only after the push. Another reviewer's thread that needs a human: reply with the decision needed and who should make it, and leave it unresolved.
5. To have MergeLoop's reviewer settle one of its findings now, rather than on the next push, reply in its thread mentioning MergeLoop's App, as the review's summary names it, with why it doesn't apply. The agent reads the code again, answers in the thread, and withdraws the finding if your reason holds.

Leave each thread MergeLoop's review marked **For a human** to them: resolving it answers it as fine as it is. Never approve a PR you built. Then run `{then}`.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{EjectedView, HoldView, MergedView, QueueView, RepoStatusView};

    /// The need as an agent that acts on GitHub itself reads it.
    fn view(
        status: PullStateView,
        exit: Option<&ExitRecord>,
        newest: Option<&EventRecord>,
        after: Option<&str>,
    ) -> NeedView {
        super::view(status, exit, newest, after, Reader::Agent)
    }

    fn status(queue: Option<QueueView>) -> PullStateView {
        PullStateView {
            repo: "o/r".into(),
            number: 6,
            url: String::new(),
            mergeloop: RepoStatusView {
                state: RepoStateView::On,
                installed: true,
                has_config: true,
                forced: None,
                config_error: None,
                hold: None,
                missing_key: None,
            },
            queue,
            merged: None::<MergedView>,
            ejected: None::<EjectedView>,
        }
    }

    fn held(reason: HoldReasonView, layer: Option<u32>) -> Option<QueueView> {
        Some(QueueView {
            trunk: "main".into(),
            state: QueueStateView::Held,
            stage: None,
            hold: Some(HoldView {
                reason,
                layer,
                message: "the hold".into(),
            }),
            priority: false,
            position: 2,
            stack: None,
            batch_pr: None,
        })
    }

    fn exit(id: i64, outcome: &str) -> ExitRecord {
        ExitRecord {
            id,
            repo: "o/r".into(),
            trunk: "main".into(),
            pr: 6,
            outcome: outcome.into(),
            detail: "checks failed".into(),
            summary: Default::default(),
            enqueued_at: None,
            steps: None,
            at: String::new(),
        }
    }

    fn event(kind: &str) -> EventRecord {
        EventRecord {
            repo: "o/r".into(),
            pr: 6,
            at: "2026-11-04T10:00:00Z".into(),
            kind: kind.into(),
            message: String::new(),
        }
    }

    #[test]
    fn a_hold_is_the_authors_or_a_humans_or_lifts_by_itself() {
        let kind = |reason| need(&status(held(reason, None)), None).kind;
        assert_eq!(kind(HoldReasonView::ReviewChanges), NeedKind::Threads);
        assert_eq!(kind(HoldReasonView::UnresolvedThreads), NeedKind::Threads);
        assert_eq!(kind(HoldReasonView::ChangesRequested), NeedKind::Changes);
        assert_eq!(kind(HoldReasonView::Draft), NeedKind::Draft);
        assert_eq!(kind(HoldReasonView::ReviewAsks), NeedKind::Human);
        assert_eq!(kind(HoldReasonView::ReviewRequired), NeedKind::Human);
        assert_eq!(kind(HoldReasonView::Reviewing), NeedKind::Wait);
        assert_eq!(kind(HoldReasonView::PrepFixing), NeedKind::Wait);
    }

    #[test]
    fn a_pr_out_of_the_queue_needs_what_its_exit_says() {
        let kind = |exit: Option<ExitRecord>| need(&status(None), exit.as_ref()).kind;
        assert_eq!(kind(None), NeedKind::Queue);
        assert_eq!(kind(Some(exit(1, "merged"))), NeedKind::Merged);
        assert_eq!(kind(Some(exit(1, "ejected"))), NeedKind::Ejected);
        assert_eq!(kind(Some(exit(1, "dequeued"))), NeedKind::Removed);
    }

    #[test]
    fn a_repo_whose_queue_does_not_run_needs_a_human() {
        let mut off = status(None);
        off.mergeloop.state = RepoStateView::Off;
        let v = view(off, None, None, None);
        assert_eq!((v.needs, v.exit_code), (NeedKind::Off, 2));
    }

    #[test]
    fn the_same_need_after_a_new_event_is_a_new_one() {
        let threads = || status(held(HoldReasonView::ReviewChanges, None));
        let first = view(threads(), None, Some(&event("review-sent-back")), None);
        let again = view(threads(), None, Some(&event("review-sent-back")), None);
        assert_eq!(first.id, again.id);
        let pushed = view(threads(), None, Some(&event("head-moved")), None);
        assert_ne!(first.id, pushed.id);
        assert_eq!(first.then, format!("mergeloop o/r#6 --after {}", first.id));
        assert_eq!(first.exit_code, 0);
    }

    #[test]
    fn an_ejection_the_agent_saw_is_queued_again() {
        let ejected = view(status(None), Some(&exit(4, "ejected")), None, None);
        assert_eq!((ejected.needs, ejected.exit_code), (NeedKind::Ejected, 11));
        let again = view(
            status(None),
            Some(&exit(4, "ejected")),
            None,
            Some(&ejected.id),
        );
        assert_eq!(again.needs, NeedKind::Queue);
        assert_ne!(again.id, ejected.id);
        // Queued, and not in a lane yet: still that need, which the agent saw.
        let queued = view(
            status(None),
            Some(&exit(4, "ejected")),
            None,
            Some(&again.id),
        );
        assert_eq!((queued.needs, queued.id), (NeedKind::Queue, again.id));
    }

    #[test]
    fn acknowledging_removal_does_not_authorize_requeue() {
        let removed = view(status(None), Some(&exit(8, "dequeued")), None, None);
        assert_eq!((removed.needs, removed.exit_code), (NeedKind::Removed, 12));
        // `after` acknowledges a need; the tool describes it as waiting for a newer one.
        // The agent has not received permission to reverse the user's removal.
        let acknowledged = view(
            status(None),
            Some(&exit(8, "dequeued")),
            None,
            Some(&removed.id),
        );
        assert_ne!(
            acknowledged.needs,
            NeedKind::Queue,
            "acknowledging a manual removal must not tell the CLI to queue the PR again"
        );
        assert!(removed.brief.contains("mergeloop call queue_pull"));
    }

    #[test]
    fn a_layers_need_points_at_the_layer() {
        let v = view(
            status(held(HoldReasonView::UnresolvedThreads, Some(4))),
            None,
            None,
            None,
        );
        assert_eq!(v.layer, Some(4));
        assert!(v.brief.contains("on #4, a layer of its stack"));
        assert!(v.brief.contains("pullRequest(number: 4)"));
        assert!(v.brief.ends_with(&format!("Then run `{}`.", v.then)));
    }

    #[test]
    fn the_daemons_agent_changes_files_only_and_ends_with_what_the_daemon_posts() {
        for (reason, kind) in [
            (HoldReasonView::ReviewChanges, NeedKind::Threads),
            (HoldReasonView::ChangesRequested, NeedKind::Changes),
        ] {
            let v = super::view(
                status(held(reason, Some(4))),
                None,
                None,
                None,
                Reader::Daemon,
            );
            assert_eq!(v.needs, kind);
            let brief = &v.brief;
            for said in [
                "do not commit, push, reply or resolve",
                "MERGELOOP_REPLY: <thread id>",
                "MERGELOOP_COMMIT: <subject>",
                "pullRequest(number: 4)",
                r#"repository(owner: "o", name: "r")"#,
            ] {
                assert!(brief.contains(said), "{kind:?} {said}:\n{brief}");
            }
            for unsaid in ["push once", "Then run", "mentioning MergeLoop's App", "{"] {
                assert!(
                    !brief
                        .replace(&threads("o/r", 4), "")
                        .replace(&args("o/r", 4), "")
                        .contains(unsaid),
                    "{kind:?} {unsaid}:\n{brief}"
                );
            }
        }
        let threads = super::view(
            status(held(HoldReasonView::UnresolvedThreads, None)),
            None,
            None,
            None,
            Reader::Daemon,
        );
        assert!(threads.brief.contains("MERGELOOP_RESOLVE: <thread id>"));
        // Needs no agent works are as an agent reads them.
        let human = super::view(
            status(held(HoldReasonView::ReviewRequired, None)),
            None,
            None,
            None,
            Reader::Daemon,
        );
        assert_eq!(
            human.brief,
            view(
                status(held(HoldReasonView::ReviewRequired, None)),
                None,
                None,
                None
            )
            .brief
        );
    }
}
