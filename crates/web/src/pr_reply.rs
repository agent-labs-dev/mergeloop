//! What people say on GitHub about a PR the agent reviewed. Its asks are answered where they
//! are, each in its thread: resolved by someone who may push, it is fine as it is, and a reply
//! there from anyone else who may push but did not open the PR asks for the change it says.
//! Approving the PR's reviewed head answers every ask as fine, and requesting changes on it
//! sends them all to the author. A reply in a finding's thread that mentions the App puts it a
//! question, which it answers in the thread: a human's about an ask, or the author's about a
//! finding sent back to them. A top-level comment answers nothing: the PR is told once, on
//! GitHub, how to answer, so nobody waits on a hold they think they cleared.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use mergepatrol_core::{
    Answer, FINDING_MARKS, PrNumber, REPLY_MARKS, RepoName, RepoSettings, ReviewStatus,
    finding_title, thread_title,
};
use mergepatrol_github::{App, Installation, PullRequest, ReviewThread};
use mergepatrol_store::{ReviewAnswers, ReviewRecord};
use serde_json::Value;

use crate::AppState;
use crate::gate::merge_requested;
use crate::review::{ANSWERED_MARKS, LINE_MARKS};

/// A human's word on a PR, as a delivery brings it.
#[derive(Debug, PartialEq)]
pub(crate) struct Reply {
    pub number: PrNumber,
    pub login: String,
    pub kind: ReplyKind,
}

#[derive(Debug, PartialEq)]
pub(crate) enum ReplyKind {
    /// A review that approves the PR at `commit`.
    Approved { commit: String },
    /// A review that asks for changes to the PR at `commit`, saying `note`.
    ChangesRequested { commit: String, note: String },
    /// A reply in a review thread, or a thread resolved or unresolved: the threads say what.
    Thread,
    /// A comment on the PR, or a review with a body that neither approves nor asks for changes.
    Said,
}

/// The reply the delivery brings, if it brings a human's. What the dashboard posts for its
/// answers is not one: the answers are stored already.
pub(crate) fn reply(event: Option<&str>, payload: &Value) -> Option<Reply> {
    let pr = &payload["pull_request"]["number"];
    let (number, user, body, kind) = match (event, payload["action"].as_str()) {
        (Some("pull_request_review"), Some("submitted")) => {
            let review = &payload["review"];
            let body = review["body"].as_str().unwrap_or_default();
            let commit = || review["commit_id"].as_str().map(str::to_owned);
            let kind = match review["state"].as_str() {
                Some("approved") => ReplyKind::Approved { commit: commit()? },
                Some("changes_requested") => ReplyKind::ChangesRequested {
                    commit: commit()?,
                    note: body.trim().to_owned(),
                },
                // A review of replies in threads only: each reply's own delivery reads it.
                _ if body.trim().is_empty() => return None,
                _ => ReplyKind::Said,
            };
            (pr, &review["user"], body, kind)
        }
        (Some("pull_request_review_comment"), Some("created")) => {
            let comment = &payload["comment"];
            let body = comment["body"].as_str().unwrap_or_default();
            (pr, &comment["user"], body, ReplyKind::Thread)
        }
        (Some("pull_request_review_thread"), Some("resolved" | "unresolved")) => {
            (pr, &payload["sender"], "", ReplyKind::Thread)
        }
        (Some("issue_comment"), Some("created")) if !payload["issue"]["pull_request"].is_null() => {
            let body = payload["comment"]["body"].as_str().unwrap_or_default();
            (
                &payload["issue"]["number"],
                &payload["comment"]["user"],
                body,
                ReplyKind::Said,
            )
        }
        _ => return None,
    };
    if user["type"] == "Bot" {
        return None;
    }
    if ANSWERED_MARKS
        .iter()
        .chain(&LINE_MARKS)
        .chain(&REPLY_MARKS)
        .any(|m| body.contains(m))
    {
        return None;
    }
    Some(Reply {
        number: number.as_u64()?,
        login: user["login"].as_str()?.to_owned(),
        kind,
    })
}

/// Acts on a reply to a PR the agent reviewed, once the PR has been told to merge: reads its
/// threads for answers and questions, takes an approval or a request for changes of the head it
/// reviewed from someone who may push, and tells the PR how to answer otherwise.
pub(crate) async fn heard(
    app: &AppState,
    installation: u64,
    repo: &RepoName,
    reply: Reply,
) -> Result<()> {
    let Some(review) = app
        .store
        .latest_review(installation, repo, reply.number)
        .await?
    else {
        return Ok(());
    };
    // Only a review that asks, or sent something back, has threads to answer or ask about.
    if review.status != ReviewStatus::Asks && !review.findings.iter().any(|f| f.sent_back) {
        return Ok(());
    }
    let creds = app
        .app_credentials()
        .await?
        .context("no GitHub App is set up")?;
    let gh = App::from_credentials(&creds)?.installation(installation);
    // The queue acts on a PR only after the PR has been told to merge. A review or a comment on
    // one that has not is state only: nothing is nudged, asked, or answered.
    let pr = gh.pull_request(repo, reply.number).await?;
    if !told_to_merge(app, &gh, repo, &pr).await? {
        return Ok(());
    }
    let mut writers = Writers::new(&gh, repo);
    let missed = match reply.kind {
        ReplyKind::Thread => {
            let mention = format!("@{}", creds.slug);
            return in_threads(app, &gh, &mut writers, &review, &pr, &mention).await;
        }
        _ if review.status != ReviewStatus::Asks => return Ok(()),
        ReplyKind::Said => Missed::Said,
        ReplyKind::Approved { commit } | ReplyKind::ChangesRequested { commit, .. }
            if commit != review.head_sha =>
        {
            Missed::OtherCommit { commit }
        }
        _ if !writers.may(&reply.login).await? => Missed::NoWrite,
        ReplyKind::Approved { .. } => {
            let answers = review.asks().map(|f| (f.finding.id.clone(), Answer::Fine));
            let answered = answered(&review, answers.collect(), &reply.login, String::new());
            return settle(app, &review, &answered).await;
        }
        ReplyKind::ChangesRequested { note, .. } => {
            let note = match note.is_empty() {
                true => "Asked for changes on GitHub.".to_owned(),
                false => note,
            };
            let answers = review.asks().map(|f| {
                let change = Answer::Change { note: note.clone() };
                (f.finding.id.clone(), change)
            });
            // A review that asks nothing, as one the agent could not make, is sent back whole.
            let answered = answered(&review, answers.collect(), &reply.login, note);
            let answered = ReviewAnswers {
                status: ReviewStatus::Changes,
                ..answered
            };
            return settle(app, &review, &answered).await;
        }
    };
    if !app.store.nudge_review(review.id).await? {
        return Ok(());
    }
    let posted = async {
        let mention = format!("@{}", creds.slug);
        let body = nudge(&review, &reply.login, &missed, &mention);
        gh.comment(repo, reply.number, &body).await
    };
    if let Err(e) = posted.await {
        // Not told after all: a later reply tries again.
        app.store.release_nudge(review.id).await?;
        return Err(e);
    }
    tracing::info!(installation, %repo, number = reply.number, by = %reply.login, "told the PR how to answer the agent's review");
    Ok(())
}

/// Whether the PR has been told to merge, by the repo's trigger label as its settings name it.
async fn told_to_merge(
    app: &AppState,
    gh: &Installation,
    repo: &RepoName,
    pr: &PullRequest,
) -> Result<bool> {
    let defaults = app.store.inherited(gh.id, &app.repo_defaults).await?;
    let found = gh.config_file(repo).await?;
    let settings = RepoSettings::from_file_at(found.at, found.file.text.as_deref(), &defaults)
        .unwrap_or(defaults);
    merge_requested(app.store.as_ref(), gh, repo, pr, &settings.label).await
}

/// Who may answer the review, and ask the agent about it: those who may push to the repo.
/// GitHub is asked once per login.
struct Writers<'a> {
    gh: &'a Installation,
    repo: &'a RepoName,
    known: HashMap<String, bool>,
}

impl<'a> Writers<'a> {
    fn new(gh: &'a Installation, repo: &'a RepoName) -> Self {
        Self {
            gh,
            repo,
            known: HashMap::new(),
        }
    }

    async fn may(&mut self, login: &str) -> Result<bool> {
        let key = login.to_lowercase();
        if let Some(&may) = self.known.get(&key) {
            return Ok(may);
        }
        let may = self.gh.can_write(self.repo, login).await?;
        self.known.insert(key, may);
        Ok(may)
    }
}

/// Reads the review's threads on GitHub: puts each new question to the agent, and settles the
/// review once every ask has an answer.
async fn in_threads(
    app: &AppState,
    gh: &Installation,
    writers: &mut Writers<'_>,
    review: &ReviewRecord,
    pr: &PullRequest,
    mention: &str,
) -> Result<()> {
    let threads = gh
        .threads_marked(&review.repo, review.number, &FINDING_MARKS)
        .await?;
    let said = read(review, &threads, mention);
    for q in said.questions {
        let asked_before = review
            .questions
            .iter()
            .any(|old| old.finding == q.finding && old.question == q.text && old.by == q.by);
        if asked_before || !writers.may(&q.by).await? {
            continue;
        }
        app.store
            .add_question(review.id, &q.finding, &q.text, &q.by)
            .await?;
        tracing::info!(repo = %review.repo, number = review.number, finding = %q.finding, by = %q.by, "asked the agent on GitHub");
    }
    if review.status != ReviewStatus::Asks {
        return Ok(());
    }
    let mut answers = BTreeMap::new();
    let mut last = String::new();
    for a in said.answers {
        // The author's reply is theirs to act on: the change goes in their push.
        let reply = matches!(a.answer, Answer::Change { .. });
        if (reply && pr.authored_by(&a.by)) || !writers.may(&a.by).await? {
            continue;
        }
        answers.insert(a.finding, a.answer);
        last = a.by;
    }
    if !every_ask_answered(review, &answers) {
        return Ok(());
    }
    settle(
        app,
        review,
        &answered(review, answers, &last, String::new()),
    )
    .await
}

/// Whether the threads answer the review: each of its asks has an answer. A review that asks
/// nothing in a thread, as one the agent could not make, is answered only by a review of the PR.
fn every_ask_answered(review: &ReviewRecord, answers: &BTreeMap<String, Answer>) -> bool {
    let mut asks = review.asks().peekable();
    asks.peek().is_some() && asks.all(|f| answers.contains_key(&f.finding.id))
}

/// What the threads say: each ask's answer, if it has one, and the questions put to the App.
#[derive(Debug, Default, PartialEq)]
struct Said {
    answers: Vec<ThreadAnswer>,
    questions: Vec<ThreadQuestion>,
}

#[derive(Debug, PartialEq)]
struct ThreadAnswer {
    finding: String,
    answer: Answer,
    by: String,
}

#[derive(Debug, PartialEq)]
struct ThreadQuestion {
    finding: String,
    text: String,
    by: String,
}

/// Reads each finding's thread as it is now. Resolved, an ask is fine as it is. Open, a human's
/// last reply asks the agent a question when it mentions the App, and asks for a change when it
/// is about an ask. Who may answer or ask is for the caller to check.
fn read(review: &ReviewRecord, threads: &[ReviewThread], mention: &str) -> Said {
    let mut said = Said::default();
    for f in review.findings.iter().filter(|f| f.asked || f.sent_back) {
        let title = thread_title(&f.finding.title);
        let Some(t) = threads
            .iter()
            .find(|t| finding_title(&t.body).as_ref() == Some(&title))
        else {
            continue;
        };
        let id = f.finding.id.clone();
        if t.resolved {
            if let (true, Some(by)) = (f.asked, &t.resolved_by) {
                let (answer, by) = (Answer::Fine, by.clone());
                said.answers.push(ThreadAnswer {
                    finding: id,
                    answer,
                    by,
                });
            }
            continue;
        }
        let Some((by, body)) = &t.reply else {
            continue;
        };
        if by.is_empty() || REPLY_MARKS.iter().any(|m| body.contains(m)) {
            continue;
        }
        let text = body.trim().to_owned();
        if mentions(body, mention) {
            let by = by.clone();
            said.questions.push(ThreadQuestion {
                finding: id,
                text,
                by,
            });
        } else if f.asked {
            let answer = Answer::Change { note: text };
            said.answers.push(ThreadAnswer {
                finding: id,
                answer,
                by: by.clone(),
            });
        }
    }
    said
}

/// Whether `body` mentions `mention`, an `@login`, as GitHub would: whole, in any case.
fn mentions(body: &str, mention: &str) -> bool {
    let (body, mention) = (body.to_lowercase(), mention.to_lowercase());
    body.match_indices(&mention).any(|(at, _)| {
        let before = body[..at].chars().next_back();
        let after = body[at + mention.len()..].chars().next();
        let word = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

/// The review answered so: it holds the PR for the author while any change is asked for, or
/// anything it sent back them is still theirs, and lets it on otherwise.
fn answered(
    review: &ReviewRecord,
    answers: BTreeMap<String, Answer>,
    by: &str,
    note: String,
) -> ReviewAnswers {
    let changes = answers.values().any(|a| matches!(a, Answer::Change { .. }));
    let owed = review.findings.iter().any(|f| f.sent_back);
    ReviewAnswers {
        answers,
        by: by.to_owned(),
        note,
        status: match changes || owed {
            true => ReviewStatus::Changes,
            false => ReviewStatus::Approved,
        },
    }
}

/// Stores the answers, and wakes the queue for them.
async fn settle(app: &AppState, review: &ReviewRecord, answered: &ReviewAnswers) -> Result<()> {
    let (installation, repo, number) = (review.installation_id, &review.repo, review.number);
    if app.store.answer_review(review.id, answered).await? {
        let status = answered.status.as_str();
        tracing::info!(installation, %repo, number, by = %answered.by, status, "the agent's review is answered on GitHub");
        app.store
            .inbox_reviewed(installation, repo, number, &answered.by)
            .await?;
        app.wake.wake(installation, repo.clone());
    }
    Ok(())
}

/// Why a reply did not answer the review.
enum Missed {
    /// A comment, or a review that neither approves nor asks for changes.
    Said,
    /// A review of another commit than the one the agent reviewed.
    OtherCommit { commit: String },
    /// A review from someone who may not push to the repo.
    NoWrite,
}

/// What the PR is told: that its review still holds it, why the reply did not clear it, and
/// how to.
fn nudge(review: &ReviewRecord, login: &str, missed: &Missed, mention: &str) -> String {
    let asks = review.asks().count();
    let what = match asks {
        // The agent could not review: a human reviews the whole PR instead.
        0 => "the agent could not review it, so it waits for a human's review".to_owned(),
        1 => "it asks a human about 1 thing".to_owned(),
        n => format!("it asks a human about {n} things"),
    };
    let answer = match asks {
        0 => "Approve the PR, or request changes".to_owned(),
        _ => format!(
            "Answer each ask in its thread: resolve it if it's fine as it is, or reply with the change it needs. Reply there with {mention} to ask the agent about it first. Approving the PR answers them all as fine, and requesting changes sends them all to the author"
        ),
    };
    let how = match missed {
        Missed::Said => format!("Comments here don't answer it. {answer}."),
        Missed::OtherCommit { commit } => format!(
            "Your review is of `{}`, but the agent reviewed `{}`, so it doesn't answer it. {answer}.",
            short(commit),
            short(&review.head_sha),
        ),
        Missed::NoWrite => {
            "Your review doesn't answer it: only someone who can push to the repo may. Ask someone who can.".to_owned()
        }
    };
    format!(
        "@{login}, MergeLoop's review still holds this PR: {what}. {how} The PR keeps its place in the queue meanwhile."
    )
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use mergepatrol_core::Judged;
    use serde_json::json;

    use super::*;

    fn finding(id: &str, title: &str, asked: bool, sent_back: bool) -> Judged {
        serde_json::from_value(json!({
            "id": id, "kind": "other", "title": title, "confidence": 70, "lean": "change",
            "finding": "", "asked": asked, "sent_back": sent_back, "threaded": true, "bar": 80,
        }))
        .unwrap()
    }

    fn review(status: ReviewStatus, findings: Vec<Judged>) -> ReviewRecord {
        ReviewRecord {
            id: 1,
            installation_id: 1,
            repo: RepoName::parse("o/r").unwrap(),
            number: 5,
            head_sha: "h1".into(),
            since_head: None,
            status,
            claim: 0,
            error: None,
            attempts: 0,
            summary: String::new(),
            findings,
            asked: Vec::new(),
            answers: Default::default(),
            answered_by: None,
            note: String::new(),
            questions: Vec::new(),
            waited_secs: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn thread(title: &str, resolved_by: Option<&str>, reply: Option<(&str, &str)>) -> ReviewThread {
        ReviewThread {
            id: format!("t-{title}"),
            resolved: resolved_by.is_some(),
            resolved_by: resolved_by.map(str::to_owned),
            comment_id: 1,
            body: format!(
                "**For a human**\n\n{}{} -->",
                FINDING_MARKS[0],
                thread_title(title)
            ),
            reply: reply.map(|(by, body)| (by.to_owned(), body.to_owned())),
        }
    }

    #[test]
    fn a_human_reply_on_a_pr_is_read_and_the_rest_is_not() {
        let human = json!({ "login": "ann", "type": "User" });
        let bot = json!({ "login": "mergeloop[bot]", "type": "Bot" });
        let pr = json!({ "number": 5 });
        let review = |state: &str, user: &Value, body: &str| {
            json!({ "action": "submitted", "pull_request": pr,
                    "review": { "state": state, "commit_id": "h1", "user": user, "body": body } })
        };
        let thread_event = |action: &str, sender: &Value| json!({ "action": action, "pull_request": pr, "sender": sender, "thread": {} });
        let cases = [
            (
                "pull_request_review",
                review("approved", &human, ""),
                Some(ReplyKind::Approved {
                    commit: "h1".into(),
                }),
            ),
            (
                "pull_request_review",
                review("changes_requested", &human, " no "),
                Some(ReplyKind::ChangesRequested {
                    commit: "h1".into(),
                    note: "no".into(),
                }),
            ),
            (
                "pull_request_review",
                review("commented", &human, "q1: fine"),
                Some(ReplyKind::Said),
            ),
            // Replies in threads, sent as a review: each one's own delivery reads it.
            ("pull_request_review", review("commented", &human, ""), None),
            ("pull_request_review", review("approved", &bot, ""), None),
            (
                "pull_request_review",
                review(
                    "approved",
                    &human,
                    &format!("ok\n\n<sub>{}</sub>", ANSWERED_MARKS[1]),
                ),
                None,
            ),
            (
                "pull_request_review_comment",
                json!({ "action": "created", "pull_request": pr,
                        "comment": { "user": human, "body": "this is fine" } }),
                Some(ReplyKind::Thread),
            ),
            (
                "pull_request_review_comment",
                json!({ "action": "created", "pull_request": pr,
                        "comment": { "user": human, "body": format!("x\n\n<!-- {} -->", LINE_MARKS[0]) } }),
                None,
            ),
            (
                "pull_request_review_comment",
                json!({ "action": "created", "pull_request": pr,
                        "comment": { "user": human, "body": format!("Cleared.\n\n{}", REPLY_MARKS[0]) } }),
                None,
            ),
            (
                "pull_request_review_thread",
                thread_event("resolved", &human),
                Some(ReplyKind::Thread),
            ),
            (
                "pull_request_review_thread",
                thread_event("unresolved", &human),
                Some(ReplyKind::Thread),
            ),
            // MergeLoop resolving a thread it cleared is not a human's word.
            (
                "pull_request_review_thread",
                thread_event("resolved", &bot),
                None,
            ),
            (
                "issue_comment",
                json!({ "action": "created", "issue": { "number": 5, "pull_request": {} },
                        "comment": { "user": human, "body": "1. yes 2. no" } }),
                Some(ReplyKind::Said),
            ),
            // An issue's comment, and an edit, are not.
            (
                "issue_comment",
                json!({ "action": "created", "issue": { "number": 5 },
                        "comment": { "user": human, "body": "hi" } }),
                None,
            ),
            (
                "issue_comment",
                json!({ "action": "edited", "issue": { "number": 5, "pull_request": {} },
                        "comment": { "user": human, "body": "hi" } }),
                None,
            ),
        ];
        for (event, payload, want) in cases {
            let got = reply(Some(event), &payload);
            let want = want.map(|kind| Reply {
                number: 5,
                login: "ann".into(),
                kind,
            });
            assert_eq!(got, want, "{event} {payload}");
        }
    }

    #[test]
    fn each_ask_is_answered_in_its_thread() {
        let r = review(
            ReviewStatus::Asks,
            vec![
                finding("f1", "Scope of the cache", true, false),
                finding("f2", "Who may delete", true, false),
                finding("f3", "Not asked", true, false),
                finding("f4", "Retry has no cap", false, true),
                finding("f5", "A note", false, false),
            ],
        );
        let threads = [
            // Resolved: fine as it is, by whoever resolved it.
            thread("Scope of the cache", Some("ann"), Some(("ann", "fine"))),
            // A reply in an open thread asks for the change it says.
            thread("Who may delete", None, Some(("bob", " Only admins. "))),
            // Nobody said anything yet; MergeLoop's own reply is not a human's.
            thread(
                "Not asked",
                None,
                Some((
                    "mergeloop[bot]",
                    &format!("Still holds\n\n{}", REPLY_MARKS[0]),
                )),
            ),
            // Sent back to the author: their reply is no answer, and the mention a question.
            thread(
                "Retry has no cap",
                None,
                Some(("cat", "@Merge-Loop it is capped in lib.rs")),
            ),
            // A note has no thread to read, even if one matched.
            thread("A note", Some("ann"), None),
        ];
        let said = read(&r, &threads, "@merge-loop");
        assert_eq!(
            said.answers,
            vec![
                ThreadAnswer {
                    finding: "f1".into(),
                    answer: Answer::Fine,
                    by: "ann".into(),
                },
                ThreadAnswer {
                    finding: "f2".into(),
                    answer: Answer::Change {
                        note: "Only admins.".into()
                    },
                    by: "bob".into(),
                },
            ]
        );
        assert_eq!(
            said.questions,
            vec![ThreadQuestion {
                finding: "f4".into(),
                text: "@Merge-Loop it is capped in lib.rs".into(),
                by: "cat".into(),
            }]
        );
    }

    #[test]
    fn a_reply_that_mentions_the_app_asks_rather_than_answers() {
        let r = review(
            ReviewStatus::Asks,
            vec![finding("f1", "Scope", true, false)],
        );
        let threads = [thread(
            "Scope",
            None,
            Some(("ann", "@merge-loop why is this a problem?")),
        )];
        let said = read(&r, &threads, "@merge-loop");
        assert!(said.answers.is_empty());
        assert_eq!(said.questions.len(), 1);
    }

    #[test]
    fn a_mention_is_the_whole_login() {
        assert!(mentions("@merge-loop look", "@merge-loop"));
        assert!(mentions("cc @Merge-Loop.", "@merge-loop"));
        assert!(mentions("(@merge-loop)", "@merge-loop"));
        assert!(!mentions("@merge-loop-dev look", "@merge-loop"));
        assert!(!mentions("me@merge-loop", "@merge-loop"));
        assert!(!mentions("merge-loop", "@merge-loop"));
    }

    #[test]
    fn answers_let_the_pr_on_unless_a_change_is_asked_or_owed() {
        let fine = BTreeMap::from([("f1".to_owned(), Answer::Fine)]);
        let change = BTreeMap::from([("f1".to_owned(), Answer::Change { note: "x".into() })]);
        let asks = review(ReviewStatus::Asks, vec![finding("f1", "a", true, false)]);
        let status = |r: &ReviewRecord, a: &BTreeMap<String, Answer>| {
            answered(r, a.clone(), "ann", String::new()).status
        };
        assert_eq!(status(&asks, &fine), ReviewStatus::Approved);
        assert_eq!(status(&asks, &change), ReviewStatus::Changes);
        // What it sent back the author still holds the PR for their push.
        let owed = review(
            ReviewStatus::Asks,
            vec![
                finding("f1", "a", true, false),
                finding("f2", "b", false, true),
            ],
        );
        assert_eq!(status(&owed, &fine), ReviewStatus::Changes);
    }

    #[test]
    fn threads_answer_only_a_review_that_asks_in_them() {
        let fine = BTreeMap::from([("f1".to_owned(), Answer::Fine)]);
        let one = review(ReviewStatus::Asks, vec![finding("f1", "a", true, false)]);
        assert!(every_ask_answered(&one, &fine));
        assert!(!every_ask_answered(&one, &BTreeMap::new()));
        let two = review(
            ReviewStatus::Asks,
            vec![
                finding("f1", "a", true, false),
                finding("f2", "b", true, false),
            ],
        );
        assert!(!every_ask_answered(&two, &fine));
        // The agent could not review: its asks are none, and a thread answers nothing.
        let failed = review(ReviewStatus::Asks, Vec::new());
        assert!(!every_ask_answered(&failed, &BTreeMap::new()));
        let notes = review(ReviewStatus::Asks, vec![finding("f1", "a", false, false)]);
        assert!(!every_ask_answered(&notes, &fine));
    }
}
