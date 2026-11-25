//! GitHub webhooks. A delivery marks what it changed of what the queue reads, and wakes the
//! queue of its repo, which reads that again from GitHub. A lost delivery costs time, not state:
//! the queue reads the whole repo again now and then.

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use mergepatrol_analytics::{Analytics, WorkspaceInfo};
use mergepatrol_core::{ConfigPath, PrNumber, RepoName, RepoSettings};
use mergepatrol_store::{BRANCH_RULES, Change, RepoState};
use serde_json::Value;
use sha2::Sha256;

use crate::{ApiError, AppState};

/// GitHub's own cap on a delivery. A big push comes close; axum's default is 2 MB.
pub(crate) const MAX_BODY_BYTES: usize = 25 * 1024 * 1024;

/// Implemented by the queue.
pub trait Wake: Send + Sync {
    fn wake(&self, installation: u64, repo: RepoName);

    /// PR `number` was pushed to, and is at `head` now: its jobs for another head stop, once
    /// GitHub shows it moved.
    fn pushed(&self, installation: u64, repo: &RepoName, number: PrNumber, head: &str);
}

pub(crate) async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let Some(secret) = state
        .app_credentials()
        .await?
        .and_then(|app| app.webhook_secret)
    else {
        return Err(ApiError::Status(
            StatusCode::NOT_FOUND,
            "webhooks are not set up".into(),
        ));
    };
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !signed(&secret, &body, signature) {
        return Err(ApiError::Status(
            StatusCode::UNAUTHORIZED,
            "bad signature".into(),
        ));
    }

    let payload: Value =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("not JSON"))?;
    let event = headers.get("x-github-event").and_then(|v| v.to_str().ok());
    if event == Some("installation") {
        installation(&state.analytics, &payload);
    }
    let installation = payload["installation"]["id"].as_u64();
    let repo = payload["repository"]["full_name"]
        .as_str()
        .and_then(RepoName::parse);
    if let (Some(installation), Some(repo)) = (installation, repo) {
        tracing::debug!(event, installation, %repo, "webhook");
        // Issues are the build lane's, and nothing the queue reads.
        if event == Some("issues") {
            return crate::builds::heard(&state, installation, &repo, &payload)
                .await
                .map(|()| StatusCode::NO_CONTENT);
        }
        let mut stored = Ok(());
        if event == Some("push")
            && let Some(has) = config_after_push(&payload)
        {
            stored = state.store.set_has_config(installation, &repo, has).await;
        }
        if event == Some("pull_request")
            && let Some((pr, label, head)) = fork_labeled(&payload)
        {
            stored = state
                .store
                .set_label_head(installation, &repo, pr, label, head)
                .await;
        }
        for change in changed(event, &payload) {
            if stored.is_ok() {
                stored = state
                    .store
                    .github_changed(installation, &repo, &change)
                    .await;
            }
        }
        if event == Some("pull_request")
            && let Some((pr, label)) = labeled(&payload)
            && let Err(e) = tell_off(&state, installation, &repo, pr, label).await
        {
            tracing::warn!(installation, %repo, pr, error = %format!("{e:#}"), "could not tell a labelled PR why its repo does not queue it");
        }
        if let Some(reply) = crate::pr_reply::reply(event, &payload)
            && let Err(e) = crate::pr_reply::heard(&state, installation, &repo, reply).await
        {
            // Lost here, a review is lost, but not the threads' answers: the next delivery
            // about the PR reads them all again.
            tracing::warn!(installation, %repo, error = %format!("{e:#}"), "could not act on a reply to the agent's review");
        }
        if event == Some("pull_request")
            && let Some((number, head)) = pushed(&payload)
        {
            state.wake.pushed(installation, &repo, number, head);
        }
        // The queue reads the rest from GitHub, so it wakes even when the write failed. The
        // error still goes back, so the delivery shows as failed on GitHub, to send again.
        state.wake.wake(installation, repo);
        stored?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// An open PR a label was just put on: its number, and the label.
fn labeled(payload: &Value) -> Option<(PrNumber, &str)> {
    let pr = &payload["pull_request"];
    if payload["action"] != "labeled" || pr["state"] != "open" {
        return None;
    }
    Some((pr["number"].as_u64()?, payload["label"]["name"].as_str()?))
}

/// Tells a PR labelled to merge, in a repo whose queue does not run, why nothing queues it: the
/// queue reads only the repos it runs, so it never would.
async fn tell_off(
    app: &AppState,
    installation: u64,
    repo: &RepoName,
    pr: PrNumber,
    label: &str,
) -> anyhow::Result<()> {
    let state = app.store.repo_state(installation, repo).await?;
    if state.as_ref().is_some_and(RepoState::on) {
        return Ok(());
    }
    let creds = app
        .app_credentials()
        .await?
        .context("no GitHub App is set up")?;
    let gh = app.github.app(&creds)?.installation(installation);
    let found = gh.config_file(repo).await?;
    let defaults = app
        .store
        .inherited(installation, &app.repo_defaults)
        .await?;
    let file = found.file.text.as_deref();
    let off = Off::of(state.as_ref(), found.at, file, &defaults, label);
    match off {
        Some(off) => gh.sticky_comment(repo, pr, &off.comment(repo, label)).await,
        None => Ok(()),
    }
}

/// Why a repo's queue does not run, for a PR labelled with its trigger label.
#[derive(Debug, PartialEq, Eq)]
enum Off {
    /// The default branch has no config file.
    NoConfig,
    /// The dashboard turned the queue off.
    TurnedOff,
}

impl Off {
    /// Why, when `label` is the repo's trigger label and its queue is off. `None` for any other
    /// label, and for a repo with a config file the queue has not read yet: it runs once it does.
    fn of(
        state: Option<&RepoState>,
        at: ConfigPath,
        file: Option<&str>,
        defaults: &RepoSettings,
        label: &str,
    ) -> Option<Off> {
        // Without a config file, either path's label is one a user may have meant.
        let settings: Vec<RepoSettings> = match file {
            Some(_) => vec![
                RepoSettings::from_file_at(at, file, defaults)
                    .unwrap_or_else(|_| at.inherit(defaults)),
            ],
            None => ConfigPath::ALL
                .iter()
                .map(|p| p.inherit(defaults))
                .collect(),
        };
        // The trigger label queues a PR, and its priority form queues it first.
        let queues = |s: &RepoSettings| {
            let l = s.labels();
            [l.trigger, l.priority]
                .iter()
                .any(|t| t.eq_ignore_ascii_case(label))
        };
        if !settings.iter().any(queues) {
            return None;
        }
        if state.is_some_and(|s| s.enabled == Some(false)) {
            Some(Off::TurnedOff)
        } else {
            file.is_none().then_some(Off::NoConfig)
        }
    }

    fn comment(&self, repo: &RepoName, label: &str) -> String {
        let why = match self {
            Off::NoConfig => format!(
                "{repo} has no `{}` on its default branch, so its queue is off. Add that file, or turn the queue on in the MergeLoop dashboard",
                ConfigPath::Current.path()
            ),
            Off::TurnedOff => format!(
                "the queue of {repo} is turned off in the MergeLoop dashboard. Turn it on there"
            ),
        };
        format!(
            "**Not queued.** MergeLoop does not queue this PR: {why}, then add `{label}` again."
        )
    }
}

/// The PR and its new head, of a `pull_request` delivery for a push to it.
fn pushed(payload: &Value) -> Option<(PrNumber, &str)> {
    if payload["action"] != "synchronize" {
        return None;
    }
    let pr = &payload["pull_request"];
    Some((pr["number"].as_u64()?, pr["head"]["sha"].as_str()?))
}

/// The App was installed or uninstalled, for analytics.
fn installation(analytics: &Analytics, payload: &Value) {
    let i = &payload["installation"];
    let (Some(installation_id), Some(action), Some(sender)) = (
        i["id"].as_u64(),
        payload["action"].as_str(),
        payload["sender"]["login"].as_str(),
    ) else {
        return;
    };
    tracing::info!(
        installation = installation_id,
        action,
        sender,
        "installation"
    );
    analytics.installation(
        action,
        sender,
        &WorkspaceInfo {
            installation_id,
            account: i["account"]["login"].as_str().unwrap_or_default(),
            account_type: i["account"]["type"].as_str().unwrap_or_default(),
        },
    );
}

/// What the delivery changed of what the queue reads from GitHub.
fn changed(event: Option<&str>, payload: &Value) -> Vec<Change> {
    let value = |kind: &str, of: &Value| of.as_str().map(|v| Change::Value(format!("{kind}:{v}")));
    let pr = &payload["pull_request"];
    let changes = match event {
        Some("push") => {
            let branch = payload["ref"]
                .as_str()
                .and_then(|r| r.strip_prefix("refs/heads/"));
            let default = payload["repository"]["default_branch"].as_str();
            let trunk = branch.is_some() && branch == default;
            vec![
                branch.map(|b| Change::Value(format!("ref:{b}"))),
                trunk.then_some(Change::Repo),
            ]
        }
        // A PR that joins or leaves a stack changes the stack, of whichever PR.
        Some("pull_request") => {
            let number = pr["number"].as_u64();
            vec![
                number.map(Change::Pr),
                number.map(|n| Change::Value(format!("pull:{n}"))),
                Some(Change::ValuesUnder("stack:".into())),
            ]
        }
        Some("pull_request_review" | "pull_request_review_thread") => {
            vec![pr["number"].as_u64().map(Change::Pr)]
        }
        Some("check_run") => vec![value("checks", &payload["check_run"]["head_sha"])],
        Some("status") => vec![value("checks", &payload["sha"])],
        Some("check_suite") => {
            let suite = &payload["check_suite"];
            let mut changes = vec![
                value("checks", &suite["head_sha"]),
                value("approval", &suite["head_sha"]),
            ];
            // The checks of a PR, for the dashboard, once a suite of them completes.
            if payload["action"] == "completed" {
                let prs = suite["pull_requests"].as_array().into_iter().flatten();
                changes.extend(prs.map(|p| p["number"].as_u64().map(Change::Pr)));
            }
            changes
        }
        Some("workflow_run") => vec![value("approval", &payload["workflow_run"]["head_sha"])],
        // The repo's settings, as which merge methods it allows, and whether it is private.
        Some("repository") => vec![
            matches!(
                payload["action"].as_str(),
                Some("edited" | "privatized" | "publicized")
            )
            .then_some(Change::Repo),
        ],
        // The checks a branch requires, and the reviews.
        Some("branch_protection_rule" | "repository_ruleset") => vec![
            Some(Change::ValuesUnder(BRANCH_RULES.into())),
            Some(Change::Repo),
        ],
        _ => vec![],
    };
    changes.into_iter().flatten().collect()
}

/// Whether a push to the default branch leaves it with the config file, if the push says. It
/// has one when the last touch of either path left a file there, and none when the last touch of
/// each removed it. The periodic look at every repo catches what this misses.
fn config_after_push(payload: &Value) -> Option<bool> {
    let default = payload["repository"]["default_branch"].as_str()?;
    if payload["ref"].as_str()? != format!("refs/heads/{default}") {
        return None;
    }
    let commits = payload["commits"].as_array()?;
    let has = |commit: &Value, list: &str, path: &str| {
        commit[list]
            .as_array()
            .is_some_and(|files| files.iter().any(|f| f == path))
    };
    // Each path's state after the push: `None` where the push left it alone.
    let after: Vec<Option<bool>> = ConfigPath::ALL
        .iter()
        .map(|at| {
            commits.iter().rev().find_map(|c| {
                if has(c, "added", at.path()) || has(c, "modified", at.path()) {
                    Some(true)
                } else if has(c, "removed", at.path()) {
                    Some(false)
                } else {
                    None
                }
            })
        })
        .collect();
    if after.contains(&Some(true)) {
        Some(true)
    } else if after.iter().all(|a| *a == Some(false)) {
        Some(false)
    } else {
        None
    }
}

/// A label went on a fork PR: the PR, the label, and the head it had then. The queue first sees
/// the PR at its next poll, and a fork's author may push before that.
fn fork_labeled(payload: &Value) -> Option<(u64, &str, &str)> {
    if payload["action"].as_str()? != "labeled" {
        return None;
    }
    let pr = &payload["pull_request"];
    if pr["head"]["repo"]["full_name"] == payload["repository"]["full_name"] {
        return None;
    }
    Some((
        pr["number"].as_u64()?,
        payload["label"]["name"].as_str()?,
        pr["head"]["sha"].as_str()?,
    ))
}

/// `signature` is GitHub's `X-Hub-Signature-256`: `sha256=` and the hex HMAC of the body.
fn signed(secret: &str, body: &[u8], signature: &str) -> bool {
    let Some(hex) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = decode_hex(hex) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn decode_hex(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).ok_or(())?, 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
pub(crate) fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256={hex}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_label_on_an_open_pr_is_heard() {
        let pr = |action: &str, state: &str| json!({ "action": action, "label": { "name": "mergeloop" }, "pull_request": { "number": 5, "state": state } });
        assert_eq!(labeled(&pr("labeled", "open")), Some((5, "mergeloop")));
        assert_eq!(labeled(&pr("unlabeled", "open")), None);
        assert_eq!(labeled(&pr("labeled", "closed")), None);
    }

    #[test]
    fn a_trigger_label_on_a_repo_whose_queue_is_off_says_why() {
        let defaults = RepoSettings {
            label: mergepatrol_core::DEFAULT_LABEL.into(),
            batch_size: 8,
            agent: mergepatrol_core::AgentSettings {
                provider: Default::default(),
                model: "m".into(),
                effort: "medium".into(),
                overrides: Default::default(),
            },
            verify: Vec::new(),
            fix_ci: Default::default(),
            branches: Vec::new(),
            review: Default::default(),
        };
        let state = |enabled| RepoState {
            repo: RepoName::parse("o/r").unwrap(),
            installation_id: 1,
            enabled,
            has_config: false,
            config_error: None,
            private: None,
            agent: Vec::new(),
        };
        let none = ConfigPath::Current;
        let of = |s: Option<&RepoState>, file, label| Off::of(s, none, file, &defaults, label);
        // No config file: the label of either path is one a user meant, and nothing else is.
        assert_eq!(of(None, None, "mergeloop"), Some(Off::NoConfig));
        assert_eq!(
            of(Some(&state(None)), None, "MergePatrol"),
            Some(Off::NoConfig)
        );
        assert_eq!(of(None, None, "bug"), None);
        assert_eq!(of(None, None, "mergeloop:priority"), Some(Off::NoConfig));
        assert_eq!(of(None, None, "mergeloop:failed"), None);
        // Turned off in the dashboard: only the file's label is the trigger.
        let file = Some("label: ship\n");
        assert_eq!(
            of(Some(&state(Some(false))), file, "ship"),
            Some(Off::TurnedOff)
        );
        assert_eq!(of(Some(&state(Some(false))), file, "mergeloop"), None);
        // A config file the queue has not read yet: it runs once it does.
        assert_eq!(of(Some(&state(None)), file, "ship"), None);
        let comment = Off::NoConfig.comment(&RepoName::parse("o/r").unwrap(), "mergepatrol");
        assert!(
            comment.contains("no `.mergeloop/mergeloop.yml`"),
            "{comment}"
        );
        assert!(
            comment.ends_with("then add `mergepatrol` again."),
            "{comment}"
        );
    }

    #[test]
    fn a_ruleset_change_clears_the_branch_rules() {
        for event in ["repository_ruleset", "branch_protection_rule"] {
            let changes = changed(Some(event), &json!({}));
            assert!(changes.contains(&Change::ValuesUnder(BRANCH_RULES.into())));
        }
    }

    #[test]
    fn a_change_to_the_repos_settings_reads_the_repo_again() {
        let edited = changed(Some("repository"), &json!({ "action": "edited" }));
        assert_eq!(edited, [Change::Repo]);
        let private = changed(Some("repository"), &json!({ "action": "privatized" }));
        assert_eq!(private, [Change::Repo]);
        let renamed = changed(Some("repository"), &json!({ "action": "renamed" }));
        assert!(renamed.is_empty(), "{renamed:?}");
    }
}
