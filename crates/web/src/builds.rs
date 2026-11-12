//! The build lane: a workspace's labeled issues are tasks, which a signed-in developer's daemon
//! claims one at a time, in repos GitHub says they may push to, builds, and gives back or marks
//! stuck. The lane is off until a workspace turns it on.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use mergepatrol_core::{RepoName, RepoSettings};
use mergepatrol_github::{ConfigFile, Installation, Issue};
use mergepatrol_store::{Build, BuildSettings, BuildStatus, Claimed, Holder, IssueTask, Stage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::api::installation;
use crate::auth::{Caller, Readable, SignedIn};
use crate::{ApiError, AppState};

/// A claim not heard from for this long may be claimed again, by anyone.
const STALE_SECS: u64 = 30 * 60;
/// A claim reads a repo's labeled issues again once they are this old: webhooks get lost, and
/// an App created before `issues` joined its events gets none.
const SYNC_SECS: u64 = 5 * 60;

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BuildSettingsView {
    /// Whether the workspace's labeled issues are tasks for its developers' daemons.
    pub enabled: bool,
}

pub(crate) async fn build_settings(
    State(app): State<AppState>,
    caller: Caller,
    Path(id): Path<u64>,
) -> Result<Json<BuildSettingsView>, ApiError> {
    let id = caller.workspace(id)?;
    let settings = app.builds.build_settings(id).await?.unwrap_or_default();
    Ok(Json(BuildSettingsView {
        enabled: settings.enabled,
    }))
}

pub(crate) async fn set_build_settings(
    State(app): State<AppState>,
    caller: Caller,
    Path(id): Path<u64>,
    Json(body): Json<BuildSettingsView>,
) -> Result<Json<BuildSettingsView>, ApiError> {
    let id = caller.workspace(id)?;
    let settings = BuildSettings {
        enabled: body.enabled,
    };
    app.builds.set_build_settings(id, &settings).await?;
    tracing::info!(
        installation = id,
        enabled = settings.enabled,
        "set the build lane settings"
    );
    Ok(Json(body))
}

async fn lane_on(app: &AppState, id: u64) -> anyhow::Result<bool> {
    Ok(app
        .builds
        .build_settings(id)
        .await?
        .is_some_and(|s| s.enabled))
}

/// Every lane route but its settings refuses while the lane is off.
async fn require_lane(app: &AppState, id: u64) -> Result<(), ApiError> {
    if lane_on(app, id).await? {
        return Ok(());
    }
    Err(ApiError::Status(
        StatusCode::CONFLICT,
        "The build lane is off for this workspace. Turn it on in its settings, or with `set_build_settings`.".into(),
    ))
}

#[derive(Clone, Copy, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BuildStatusView {
    /// A task: a developer may claim it.
    Ready,
    /// A daemon holds it and builds it.
    Claimed,
    /// A daemon holds it and lands its PR.
    Landing,
    /// A daemon holds it, and its PR waits on a human.
    Waiting,
    /// Its holder gave up, and said why. It is a task again once a human takes the label off
    /// the issue and puts it back.
    Stuck,
    /// Its PR merged.
    Merged,
}

impl From<BuildStatus> for BuildStatusView {
    fn from(s: BuildStatus) -> Self {
        match s {
            BuildStatus::Ready => Self::Ready,
            BuildStatus::Claimed => Self::Claimed,
            BuildStatus::Landing => Self::Landing,
            BuildStatus::Waiting => Self::Waiting,
            BuildStatus::Stuck => Self::Stuck,
            BuildStatus::Merged => Self::Merged,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BuildStageView {
    Building,
    Reviewing,
}

impl From<Stage> for BuildStageView {
    fn from(s: Stage) -> Self {
        match s {
            Stage::Building => Self::Building,
            Stage::Reviewing => Self::Reviewing,
        }
    }
}

impl From<BuildStageView> for Stage {
    fn from(s: BuildStageView) -> Self {
        match s {
            BuildStageView::Building => Self::Building,
            BuildStageView::Reviewing => Self::Reviewing,
        }
    }
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct BuildView {
    pub id: u64,
    pub repo: String,
    pub issue: u64,
    pub title: String,
    pub status: BuildStatusView,
    /// The login that holds the claim, or held it last.
    pub holder: Option<String>,
    /// Which taking of the build `holder` has: its calls on the build pass it.
    pub claim: u64,
    pub stage: Option<BuildStageView>,
    /// Seconds since its holder was last heard from.
    pub heard_secs: Option<u64>,
    pub pr: Option<u64>,
    /// Why it is stuck.
    pub reason: Option<String>,
}

impl From<Build> for BuildView {
    fn from(b: Build) -> Self {
        Self {
            id: b.id,
            repo: b.repo.to_string(),
            issue: b.issue,
            title: b.title,
            status: b.status.into(),
            holder: b.holder,
            claim: b.claim,
            stage: b.stage.map(Into::into),
            heard_secs: b.heard_secs,
            pr: b.pr,
            reason: b.reason,
        }
    }
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct ListBuilds {
    /// Only the builds the caller's login holds.
    #[serde(default)]
    #[ts(optional)]
    pub mine: Option<bool>,
}

pub(crate) async fn list_builds(
    State(app): State<AppState>,
    caller: Caller,
    Path(id): Path<u64>,
    Query(query): Query<ListBuilds>,
) -> Result<Json<Vec<BuildView>>, ApiError> {
    let (id, readable) = caller.workspace_repos(&app, id).await?;
    require_lane(&app, id).await?;
    let login = caller.user()?.login();
    let mine = query.mine.unwrap_or(false);
    let builds = app.builds.builds(id).await?;
    Ok(Json(
        builds
            .into_iter()
            .filter(|b| readable.contains(&b.repo))
            .filter(|b| {
                !mine || (b.status.held() && b.holder.as_deref() == Some(login))
            })
            .map(BuildView::from)
            .collect(),
    ))
}

/// A git author or committer.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

/// A build just claimed: what a daemon needs to build it.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct ClaimedBuildView {
    pub build: u64,
    /// Pass it on every call on the build: a call with an older one is refused.
    pub claim: u64,
    pub repo: String,
    pub clone_url: String,
    pub issue: u64,
    pub title: String,
    pub body: String,
    pub default_branch: String,
    /// The branch to push the build to.
    pub branch: String,
    /// The repo's `verify` commands, to pass before pushing.
    pub verify: Vec<String>,
    /// The commits' author: the App's bot.
    pub author: GitIdentity,
    /// The developer, for a `Co-authored-by` trailer.
    pub co_author: GitIdentity,
    /// The build's PR, when a stale claim that had opened one was taken over.
    pub pr: Option<u64>,
}

/// Gives the caller one task in the workspace, or none.
pub(crate) async fn claim_build(
    State(app): State<AppState>,
    caller: Caller,
    Path(id): Path<u64>,
) -> Result<Json<Option<ClaimedBuildView>>, ApiError> {
    let user = caller.user()?;
    let id = user.workspace(id)?;
    require_lane(&app, id).await?;
    let pushable = pushable(&app, id, user).await?;
    let login = user.login();
    let Some(claimed) = app
        .builds
        .claim_build(id, login, &pushable, STALE_SECS)
        .await?
    else {
        return Ok(Json(None));
    };
    let build = &claimed.build;
    tracing::info!(installation = id, repo = %build.repo, issue = build.issue, login, claim = build.claim, "claimed a build");
    match claimed_view(&app, id, user, &claimed).await {
        Ok(view) => Ok(Json(Some(view))),
        Err(e) => {
            // Without what it needs, the daemon cannot build it: it is a task again.
            let holder = holder(id, build.id, login, build.claim);
            app.builds.release_build(holder, None).await?;
            Err(e)
        }
    }
}

fn holder(installation_id: u64, build: u64, login: &str, claim: u64) -> Holder<'_> {
    Holder {
        installation_id,
        build,
        login,
        claim,
    }
}

/// The repos the caller may claim a build in now: those the lane serves for them, with a build
/// to claim, that GitHub says they may push to. Reads the labeled issues that are due first.
async fn pushable(app: &AppState, id: u64, user: &SignedIn) -> Result<Vec<RepoName>, ApiError> {
    let readable = user.readable(app, id).await?;
    let repos = lane_repos(app, id, &readable).await?;
    sync(app, id, &repos).await;
    let claimable = app.builds.claimable_repos(id, STALE_SECS).await?;
    let mut pushable = Vec::new();
    for repo in repos.into_iter().filter(|r| claimable.contains(r)) {
        if user.may_push(app, &repo).await? {
            pushable.push(repo);
        }
    }
    Ok(pushable)
}

/// The workspace's repos that the lane serves for this caller: those whose queue is on, as a
/// built PR merges through it, and that the caller can read.
async fn lane_repos(
    app: &AppState,
    id: u64,
    readable: &Readable,
) -> Result<Vec<RepoName>, ApiError> {
    Ok(app
        .store
        .repos()
        .await?
        .into_iter()
        .filter(|r| r.installation_id == id && r.on() && readable.contains(&r.repo))
        .map(|r| r.repo)
        .collect())
}

/// Reads the labeled issues of each repo whose were read over `SYNC_SECS` ago. A repo that
/// fails is read again then: the claim goes on with what the store has.
async fn sync(app: &AppState, id: u64, repos: &[RepoName]) {
    for repo in repos {
        let synced = async {
            if !app.builds.take_sync(id, repo, SYNC_SECS).await? {
                return Ok(());
            }
            let gh = installation(app, id).await?;
            let label = settings(app, &gh, id, repo).await?.0.label;
            let issues = gh.labeled_issues(repo, &label).await?;
            let tasks: Vec<IssueTask> = issues.into_iter().map(task).collect();
            app.builds.sync_tasks(id, repo, &tasks).await?;
            Ok::<_, ApiError>(())
        };
        if let Err(e) = synced.await {
            tracing::warn!(installation = id, %repo, error = %e.message(), "could not read the labeled issues");
        }
    }
}

fn task(issue: Issue) -> IssueTask {
    IssueTask {
        number: issue.number,
        title: issue.title,
        body: issue.body,
        assignees: issue.assignees,
    }
}

/// The repo's settings, as its config file on the default branch sets them, and that branch.
async fn settings(
    app: &AppState,
    gh: &Installation,
    id: u64,
    repo: &RepoName,
) -> Result<(RepoSettings, String), ApiError> {
    let ConfigFile { at, file } = gh.config_file(repo).await?;
    let defaults = app.store.inherited(id, &app.repo_defaults).await?;
    let settings =
        RepoSettings::from_file_at(at, file.text.as_deref(), &defaults).map_err(|e| {
            ApiError::Status(
                StatusCode::CONFLICT,
                format!("{repo}'s config file does not parse: {e}"),
            )
        })?;
    Ok((settings, file.branch))
}

/// What the daemon needs for a build just claimed. Assigns the developer on the issue, as a
/// mirror of the claim, and takes away the assignee of the stale claim it took over.
async fn claimed_view(
    app: &AppState,
    id: u64,
    user: &SignedIn,
    claimed: &Claimed,
) -> Result<ClaimedBuildView, ApiError> {
    let build = &claimed.build;
    let gh = installation(app, id).await?;
    let (settings, default_branch) = settings(app, &gh, id, &build.repo).await?;
    let creds = app
        .app_credentials()
        .await?
        .ok_or_else(|| ApiError::Status(StatusCode::CONFLICT, "No GitHub App yet.".into()))?;
    let bot = format!("{}[bot]", creds.slug);
    let author = identity(app, &gh, &bot).await?;
    let co_author = identity(app, &gh, user.login()).await?;
    if build.mirrored {
        mirror(
            gh.assign(&build.repo, build.issue, user.login()).await,
            build,
            "assign",
        );
    }
    if let Some((before, true)) = &claimed.before
        && before != user.login()
    {
        mirror(
            gh.unassign(&build.repo, build.issue, before).await,
            build,
            "unassign",
        );
    }
    Ok(ClaimedBuildView {
        build: build.id,
        claim: build.claim,
        repo: build.repo.to_string(),
        clone_url: format!("https://github.com/{}.git", build.repo),
        issue: build.issue,
        title: build.title.clone(),
        body: build.body.clone(),
        default_branch,
        branch: format!("mergeloop/build-{}", build.issue),
        verify: settings.verify,
        author,
        co_author,
        pr: build.pr,
    })
}

/// The issue's assignees only mirror the claims, so a failure to change them fails nothing.
fn mirror(done: anyhow::Result<()>, build: &Build, what: &str) {
    if let Err(e) = done {
        tracing::warn!(repo = %build.repo, issue = build.issue, error = %format!("{e:#}"), "could not {what} on the issue");
    }
}

/// `login`'s git identity: GitHub links a commit to an account by its `noreply` email.
async fn identity(app: &AppState, gh: &Installation, login: &str) -> Result<GitIdentity, ApiError> {
    let id = match app.seen.user_ids.get(&login.to_owned()) {
        Some(id) => id,
        None => {
            let id = gh.user_id(login).await?;
            app.seen.user_ids.put(login.to_owned(), id);
            id
        }
    };
    Ok(GitIdentity {
        name: login.to_owned(),
        email: format!("{id}+{login}@users.noreply.github.com"),
    })
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct HeartbeatBuild {
    pub claim: u64,
    #[serde(default)]
    #[ts(optional)]
    pub stage: Option<BuildStageView>,
}

pub(crate) async fn heartbeat_build(
    State(app): State<AppState>,
    caller: Caller,
    Path((id, build)): Path<(u64, u64)>,
    Json(body): Json<HeartbeatBuild>,
) -> Result<Json<BuildView>, ApiError> {
    let user = caller.user()?;
    let id = user.workspace(id)?;
    require_lane(&app, id).await?;
    let held = holder(id, build, user.login(), body.claim);
    let beat = app
        .builds
        .heartbeat_build(held, body.stage.map(Into::into))
        .await?;
    Ok(Json(beat.ok_or_else(|| lost(build))?.into()))
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct ReleaseBuild {
    pub claim: u64,
    /// Marks it stuck, for this reason, which goes on the issue. Without it, the build is a
    /// task again.
    #[serde(default)]
    #[ts(optional)]
    pub stuck: Option<String>,
}

pub(crate) async fn release_build(
    State(app): State<AppState>,
    caller: Caller,
    Path((id, build)): Path<(u64, u64)>,
    Json(body): Json<ReleaseBuild>,
) -> Result<Json<BuildView>, ApiError> {
    let user = caller.user()?;
    let id = user.workspace(id)?;
    require_lane(&app, id).await?;
    let stuck = body.stuck.as_deref().map(str::trim);
    if stuck == Some("") {
        return Err(ApiError::bad_request("Say why it is stuck."));
    }
    let held = holder(id, build, user.login(), body.claim);
    let released = app
        .builds
        .release_build(held, stuck)
        .await?
        .ok_or_else(|| lost(build))?;
    tracing::info!(installation = id, repo = %released.repo, issue = released.issue, login = user.login(), stuck, "released a build");
    let gh = installation(&app, id).await?;
    released_on_github(&gh, &released, user.login(), stuck).await;
    Ok(Json(released.into()))
}

/// Says on the issue why a build is stuck, or takes away the assignee its claim added.
async fn released_on_github(gh: &Installation, released: &Build, login: &str, stuck: Option<&str>) {
    let (repo, issue) = (&released.repo, released.issue);
    match stuck {
        Some(reason) => {
            let comment = format!(
                "MergeLoop's build lane could not build this issue, on @{login}'s machine:\n\n> {}\n\nTo try again, take the build label off this issue and put it back.",
                reason.replace('\n', "\n> ")
            );
            mirror(gh.comment(repo, issue, &comment).await, released, "comment");
        }
        None if released.mirrored && released.status == BuildStatus::Ready => {
            mirror(gh.unassign(repo, issue, login).await, released, "unassign");
        }
        None => {}
    }
}

fn lost(build: u64) -> ApiError {
    ApiError::Status(
        StatusCode::CONFLICT,
        format!(
            "You no longer hold build {build}: it was given back, or its claim went stale and someone else took it. Stop working on it."
        ),
    )
}

/// An `issues` delivery: keeps the issue's build in step with it, while the lane is on and the
/// repo's queue runs.
pub(crate) async fn heard(
    app: &AppState,
    id: u64,
    repo: &RepoName,
    payload: &Value,
) -> Result<(), ApiError> {
    if !lane_on(app, id).await? {
        return Ok(());
    }
    let Some(issue) = Issue::parse(&payload["issue"]) else {
        return Ok(());
    };
    let gone = matches!(payload["action"].as_str(), Some("deleted" | "transferred"));
    if !gone && is_task(app, id, repo, &issue).await? {
        app.builds.upsert_task(id, repo, &task(issue)).await?;
    } else {
        app.builds.drop_task(id, repo, issue.number).await?;
    }
    Ok(())
}

/// Whether the issue is a task: the repo's queue runs, and the issue is open with its label.
async fn is_task(
    app: &AppState,
    id: u64,
    repo: &RepoName,
    issue: &Issue,
) -> Result<bool, ApiError> {
    if !app
        .store
        .repo_state(id, repo)
        .await?
        .is_some_and(|r| r.on())
    {
        return Ok(false);
    }
    let gh = installation(app, id).await?;
    let label = settings(app, &gh, id, repo).await?.0.label;
    Ok(issue.labeled(&label))
}
