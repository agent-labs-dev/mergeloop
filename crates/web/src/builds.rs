//! The build lane: a workspace's labeled issues are tasks, which a signed-in developer's daemon
//! claims one at a time, in repos GitHub says they may push to, builds, pushes with a token
//! minted for its claim, has the App open a PR for, and lands, or gives back or marks stuck.
//! The lane is off until a workspace turns it on.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use mergepatrol_core::{RepoName, RepoSettings};
use mergepatrol_github::{ConfigFile, Installation, Issue, LanePull, ScopedToken};
use mergepatrol_store::{
    Build, BuildSettings, BuildStatus, Claimed, Holder, IssueTask, Stage, Standing,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::api::installation;
use crate::auth::{Caller, Readable, SignedIn};
use crate::{ApiError, AppState, Refusal};

/// A claim not heard from for this long may be claimed again, by anyone.
const STALE_SECS: u64 = 30 * 60;
/// A claim reads a repo's labeled issues again once they are this old: webhooks get lost, and
/// an App created before `issues` joined its events gets none.
const SYNC_SECS: u64 = 5 * 60;
/// A sync reads at most this many pages of 100 of each list, so one claim's work is bounded. A
/// repo with more labeled issues gets the rest by webhook.
const SYNC_PAGES: usize = 10;
/// A sync checks at most this many stuck builds for their label coming back, those checked
/// longest ago first, so its work is bounded however many are stuck.
const STUCK_CHECKS: u64 = 5;

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
    Err(ApiError::Refused(
        Refusal::LaneOff,
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
    /// Its holder gave up, and said why. It is a task again once a human puts the label on the
    /// issue again, with its PR if it has one.
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
    /// With `mine`, only those this daemon of theirs holds.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
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
                b.task || b.status.held() || b.status == BuildStatus::Stuck && b.pr.is_some()
            })
            .filter(|b| !mine || (b.status.held() && b.holder.as_deref() == Some(login)))
            .filter(|b| !mine || query.daemon.is_none() || b.daemon == query.daemon)
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

#[derive(Deserialize, Default, TS)]
#[ts(export)]
pub struct ClaimBuild {
    /// Which of the caller's daemons claims: it alone holds the build, and passes the same on
    /// every later call on it. Another daemon of the same login is another holder.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
}

/// The longest name a daemon may give itself.
const DAEMON_MAX: usize = 64;

/// Gives the caller one task in the workspace, or none.
pub(crate) async fn claim_build(
    State(app): State<AppState>,
    caller: Caller,
    Path(id): Path<u64>,
    body: Option<Json<ClaimBuild>>,
) -> Result<Json<Option<ClaimedBuildView>>, ApiError> {
    let user = caller.user()?;
    let id = user.workspace(id)?;
    let ClaimBuild { daemon } = body.map(|Json(b)| b).unwrap_or_default();
    let daemon = daemon.as_deref();
    if daemon.is_some_and(|d| {
        d.is_empty()
            || d.len() > DAEMON_MAX
            || !d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) {
        return Err(ApiError::bad_request(
            "`daemon` is up to 64 letters, digits and dashes.",
        ));
    }
    require_lane(&app, id).await?;
    let pushable = pushable(&app, id, user).await?;
    let login = user.login();
    let Some(claimed) = app
        .builds
        .claim_build(id, login, daemon, &pushable, STALE_SECS)
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
            let holder = holder(id, build.id, login, daemon, build.claim);
            app.builds.release_build(holder, None).await?;
            Err(e)
        }
    }
}

fn holder<'a>(
    installation_id: u64,
    build: u64,
    login: &'a str,
    daemon: Option<&'a str>,
    claim: u64,
) -> Holder<'a> {
    Holder {
        installation_id,
        build,
        login,
        daemon,
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
            let Some(since) = app.builds.take_sync(id, repo, SYNC_SECS).await? else {
                return Ok(());
            };
            let gh = installation(app, id).await?;
            let label = settings(app, &gh, id, repo).await?.0.label;
            let listed = gh.labeled_issues(repo, &label, SYNC_PAGES).await?;
            let tasks: Vec<IssueTask> = listed.items.into_iter().map(task).collect();
            app.builds
                .sync_tasks(id, repo, &since, &tasks, listed.complete)
                .await?;
            retire(app, &gh, id, repo).await?;
            unstick(app, &gh, id, repo, &label).await
        };
        if let Err(e) = synced.await {
            tracing::warn!(installation = id, %repo, error = %e.message(), "could not read the labeled issues");
        }
    }
}

/// Ends the stale claims on the repo's builds without a PR whose issue is no task: nobody may
/// take them over, so their holder's assignment would stay on the issue for good.
async fn retire(
    app: &AppState,
    gh: &Installation,
    id: u64,
    repo: &RepoName,
) -> Result<(), ApiError> {
    for build in app.builds.retire_stale(id, repo, STALE_SECS).await? {
        tracing::info!(installation = id, %repo, issue = build.issue, holder = build.holder, "retired a stale claim on an issue that is no task");
        if let (true, Some(holder)) = (build.mirrored, &build.holder) {
            mirror(
                gh.unassign(repo, build.issue, holder).await,
                &build,
                "unassign",
            );
        }
    }
    Ok(())
}

/// Makes a task again each stuck build of the repo whose issue had its label put on again after
/// it got stuck: its issue's events say so when no webhook did. Checks at most `STUCK_CHECKS`
/// builds a pass; one that fails is logged and the rest are checked.
async fn unstick(
    app: &AppState,
    gh: &Installation,
    id: u64,
    repo: &RepoName,
    label: &str,
) -> Result<(), ApiError> {
    for build in app.builds.stuck_to_check(id, repo, STUCK_CHECKS).await? {
        let checked = async {
            let Some(at) = gh
                .last_labeled_at(repo, build.issue, label, SYNC_PAGES)
                .await?
            else {
                return anyhow::Ok(false);
            };
            app.builds
                .relabeled_since_stuck(id, build.id, build.claim, &at)
                .await
        };
        match checked.await {
            Ok(true) => {
                tracing::info!(installation = id, %repo, issue = build.issue, "a stuck build is a task again");
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(installation = id, %repo, issue = build.issue, error = %format!("{e:#}"), "could not read a stuck build's label events");
            }
        }
    }
    Ok(())
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
    let view = task_view(app, &gh, id, user, build).await?;
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
    Ok(view)
}

/// What a daemon needs to work on `build` for `user`.
async fn task_view(
    app: &AppState,
    gh: &Installation,
    id: u64,
    user: &SignedIn,
    build: &Build,
) -> Result<ClaimedBuildView, ApiError> {
    let (settings, default_branch) = settings(app, gh, id, &build.repo).await?;
    let author = identity(app, gh, &bot(app).await?).await?;
    let co_author = identity(app, gh, user.login()).await?;
    Ok(ClaimedBuildView {
        build: build.id,
        claim: build.claim,
        repo: build.repo.to_string(),
        clone_url: format!("https://github.com/{}.git", build.repo),
        issue: build.issue,
        title: build.title.clone(),
        body: build.body.clone(),
        default_branch,
        branch: branch(build.issue),
        verify: settings.verify,
        author,
        co_author,
        pr: build.pr,
    })
}

/// The App's bot's login, which opens the builds' PRs and authors their commits.
async fn bot(app: &AppState) -> Result<String, ApiError> {
    let creds = app
        .app_credentials()
        .await?
        .ok_or_else(|| ApiError::Status(StatusCode::CONFLICT, "No GitHub App yet.".into()))?;
    Ok(format!("{}[bot]", creds.slug))
}

/// The branch a build is pushed to, and its PR opened from.
fn branch(issue: u64) -> String {
    format!("mergeloop/build-{issue}")
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
    /// The daemon that claimed it, as `claim_build` was told.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
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
    let held = holder(id, build, user.login(), body.daemon.as_deref(), body.claim);
    let beat = app
        .builds
        .heartbeat_build(held, body.stage.map(Into::into))
        .await?
        .ok_or_else(|| lost(build))?;
    if !beat.live() {
        return Err(cancelled(build));
    }
    Ok(Json(beat.into()))
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct GetBuildTask {
    pub claim: u64,
    /// The daemon that claimed it, as `claim_build` was told.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
}

/// The task of a build the caller holds, as `claim_build` gave it: for a daemon that restarted
/// and lands the PRs it holds. Changes nothing.
pub(crate) async fn get_build_task(
    State(app): State<AppState>,
    caller: Caller,
    Path((id, build)): Path<(u64, u64)>,
    Query(query): Query<GetBuildTask>,
) -> Result<Json<ClaimedBuildView>, ApiError> {
    let daemon = query.daemon.as_deref();
    let (id, build) = held(&app, &caller, id, build, daemon, query.claim).await?;
    let gh = installation(&app, id).await?;
    Ok(Json(
        task_view(&app, &gh, id, caller.user()?, &build).await?,
    ))
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct ReleaseBuild {
    pub claim: u64,
    /// The daemon that claimed it, as `claim_build` was told.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
    /// Marks it stuck, for this reason, which goes on the issue. Without it, the build is given
    /// back: a task again while its issue is one, and, with a PR, anyone's to take at once.
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
    let held = holder(id, build, user.login(), body.daemon.as_deref(), body.claim);
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
    ApiError::Refused(
        Refusal::Lost,
        format!(
            "You no longer hold build {build}: it was given back or is over, or its claim went stale and someone else took it. Stop working on it."
        ),
    )
}

/// The holder of a build without a PR whose issue stopped being a task may only give it back.
fn cancelled(build: u64) -> ApiError {
    ApiError::Refused(
        Refusal::Cancelled,
        format!(
            "Build {build}'s issue was closed or lost its label before the build had a PR. Give it back with `release_build`, and stop working on it."
        ),
    )
}

/// The live build `user` holds with `claim` from `daemon`, in a workspace whose lane is on.
async fn held(
    app: &AppState,
    caller: &Caller,
    id: u64,
    build: u64,
    daemon: Option<&str>,
    claim: u64,
) -> Result<(u64, Build), ApiError> {
    let user = caller.user()?;
    let id = user.workspace(id)?;
    require_lane(app, id).await?;
    let held = holder(id, build, user.login(), daemon, claim);
    let build = app
        .builds
        .held_build(held)
        .await?
        .ok_or_else(|| lost(build))?;
    if !build.live() {
        return Err(cancelled(build.id));
    }
    Ok((id, build))
}

fn may_not_push(repo: &RepoName) -> ApiError {
    ApiError::Refused(
        Refusal::MayNotPush,
        format!("GitHub says you may not push to {repo}. Give the build back."),
    )
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct MintBuildToken {
    pub claim: u64,
    /// The daemon that claimed it, as `claim_build` was told.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
    pub scope: TokenScope,
}

/// What a build's token may do, on its repo alone.
#[derive(Clone, Copy, Debug, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TokenScope {
    /// Push the branch and act on the PR: `contents: write`, `pull_requests: write`. The
    /// daemon's own.
    Push,
    /// Read the repo and its PRs: `contents: read`, `pull_requests: read`. For an agent, which
    /// reads what anyone who comments on the PR wrote.
    Read,
}

/// A token for the build's repo alone.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct BuildTokenView {
    pub token: String,
    /// When GitHub stops taking it, an hour after it was made, as RFC 3339.
    pub expires_at: String,
}

/// Gives the holder of a live claim a token for its build's repo, of `scope`, while GitHub says
/// they may push there. Asks GitHub afresh: the token outlives any cached answer. Once minted,
/// checks the claim and the lane again, and revokes the token if either was lost meanwhile. A
/// token already given outlives a later release by up to its hour, which GitHub fixes.
pub(crate) async fn mint_build_token(
    State(app): State<AppState>,
    caller: Caller,
    Path((id, build)): Path<(u64, u64)>,
    Json(body): Json<MintBuildToken>,
) -> Result<Json<BuildTokenView>, ApiError> {
    let (id, build) = held(&app, &caller, id, build, body.daemon.as_deref(), body.claim).await?;
    require_heard(&build)?;
    let user = caller.user()?;
    require_push_now(&app, user, &build.repo).await?;
    let gh = installation(&app, id).await?;
    let ScopedToken { token, expires_at } = match body.scope {
        TokenScope::Push => gh.push_token(&build.repo).await?,
        TokenScope::Read => gh.pr_read_token(&build.repo).await?,
    };
    let held = holder(
        id,
        build.id,
        user.login(),
        body.daemon.as_deref(),
        body.claim,
    );
    still_held(&app, &gh, held, &build.repo, &token).await?;
    tracing::info!(installation = id, repo = %build.repo, issue = build.issue, login = user.login(), scope = ?body.scope, "minted a build token");
    Ok(Json(BuildTokenView { token, expires_at }))
}

/// `held` still holds its build in a lane that is on, now that `token` was minted for it: or
/// `token` is revoked, and the call refused, also when that could not be read.
async fn still_held(
    app: &AppState,
    gh: &Installation,
    held: Holder<'_>,
    repo: &RepoName,
    token: &str,
) -> Result<(), ApiError> {
    let holds = app.builds.holds(held).await;
    if matches!(holds, Ok(true)) {
        return Ok(());
    }
    // GitHub's answer to a failed revoke may echo what it was sent: only the failure is logged.
    if gh.revoke(token).await.is_err() {
        tracing::warn!(installation = held.installation_id, %repo, "could not revoke a build token");
    }
    holds?;
    require_lane(app, held.installation_id).await?;
    Err(lost(held.build))
}

/// A claim unheard for `STALE_SECS` is anyone's to take: it is no live claim.
fn require_heard(build: &Build) -> Result<(), ApiError> {
    if build.heard_secs.is_some_and(|s| s < STALE_SECS) {
        return Ok(());
    }
    Err(ApiError::Refused(
        Refusal::Unheard,
        format!(
            "Build {} was not heard from for 30 minutes, so anyone may claim it. Heartbeat it with `heartbeat_build`, then ask again.",
            build.id
        ),
    ))
}

/// GitHub says `user` may push to `repo`, asked now and not from a cache.
async fn require_push_now(
    app: &AppState,
    user: &SignedIn,
    repo: &RepoName,
) -> Result<(), ApiError> {
    let token = user.github_token(app)?;
    match app
        .github
        .as_user(&token)?
        .can_write(&repo.to_string())
        .await?
    {
        true => Ok(()),
        false => Err(may_not_push(repo)),
    }
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct OpenBuildPr {
    pub claim: u64,
    /// The daemon that claimed it, as `claim_build` was told.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
    pub title: String,
    /// What the PR does. `Closes #<issue>` goes before it.
    pub body: String,
}

/// The App opens the build's PR from its pushed branch into the default branch, ready for
/// review, labels it so it queues, and assigns the developer. Only a live claim acts, so no
/// takeover happens while it does. The PR is recorded on the build, fenced, as soon as GitHub
/// gives it, and labeled and assigned only once recorded: a PR this call opened that neither it
/// nor a later holder recorded is closed, so none is left open that no build points at. Labeling and assigning
/// are tried again on every call, and the call succeeds only once both are done on an open PR.
pub(crate) async fn open_build_pr(
    State(app): State<AppState>,
    caller: Caller,
    Path((id, build)): Path<(u64, u64)>,
    Json(body): Json<OpenBuildPr>,
) -> Result<Json<BuildView>, ApiError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("Give the PR a title."));
    }
    let (id, build) = held(&app, &caller, id, build, body.daemon.as_deref(), body.claim).await?;
    require_heard(&build)?;
    let user = caller.user()?;
    let login = user.login();
    require_push_now(&app, user, &build.repo).await?;
    let gh = installation(&app, id).await?;
    let (settings, base) = settings(&app, &gh, id, &build.repo).await?;
    let held = holder(id, build.id, login, body.daemon.as_deref(), body.claim);
    let pr = match build.pr {
        Some(pr) => still_open(&gh, &build.repo, pr).await?,
        None => {
            let (pr, created) = opened(&app, &gh, &build, &base, title, &body.body).await?;
            if app.builds.opened_pr(held, pr).await?.is_none() {
                return Err(unrecorded(&app, &gh, held, &build, pr, created).await?);
            }
            tracing::info!(installation = id, repo = %build.repo, issue = build.issue, pr, login, created, "recorded a build's PR");
            pr
        }
    };
    queued(&gh, &build.repo, pr, &settings.label, login).await?;
    let landing = app
        .builds
        .held_build(held)
        .await?
        .ok_or_else(|| lost(build.id))?;
    Ok(Json(landing.into()))
}

/// Why PR `pr` could not be recorded on `build`, after closing it when it is an orphan.
///
/// A PR this call opened is an orphan unless a later holder recorded it meanwhile: they found
/// it on the branch, and it is theirs. A PR this call found, while it still holds the build, was
/// refused because an earlier call is closing it as an orphan: it is closed here too, in case
/// that close never reached GitHub, and the next call opens a new one.
async fn unrecorded(
    app: &AppState,
    gh: &Installation,
    held: Holder<'_>,
    build: &Build,
    pr: u64,
    created: bool,
) -> Result<ApiError, ApiError> {
    let (id, repo) = (held.installation_id, &build.repo);
    let orphan = match created {
        true => app.builds.orphaned_pr(id, build.id, pr).await?,
        false => matches!(app.builds.held_build(held).await?, Some(b) if b.pr.is_none()),
    };
    if !orphan {
        return Ok(lost(build.id));
    }
    let why = format!(
        "MergeLoop closed this PR: the build that opened it was given back or taken over by someone else before it could record it. Build {} goes on without it.",
        build.id
    );
    let closed = gh.close_pr(repo, pr, &why).await;
    if let Err(e) = &closed {
        tracing::warn!(installation = id, repo = %repo, pr, error = %format!("{e:#}"), "could not close a PR no build records");
    }
    Ok(match created {
        true => lost(build.id),
        false => ApiError::Status(
            StatusCode::BAD_GATEWAY,
            format!(
                "PR #{pr} from this branch is one an earlier call could not record, so MergeLoop {} it. Call `open_build_pr` again.",
                if closed.is_ok() {
                    "closed"
                } else {
                    "is closing"
                }
            ),
        ),
    })
}

/// PR `pr`, while it is open. Closed without merging, the build cannot go on with it; merged,
/// it is over, which the caller reports.
async fn still_open(gh: &Installation, repo: &RepoName, pr: u64) -> Result<u64, ApiError> {
    let read = gh.lane_pull(repo, pr).await?;
    match (read.open, read.merged) {
        (true, _) => Ok(pr),
        (false, true) => Err(ApiError::Refused(
            Refusal::Merged,
            format!("PR #{pr} merged. Report it with `report_build` `merged`."),
        )),
        (false, false) => Err(ApiError::Status(
            StatusCode::CONFLICT,
            format!("PR #{pr} was closed without merging. Release the build as stuck, saying so."),
        )),
    }
}

/// Labels PR `pr` with `label`, so it queues, and assigns it to `login`. Both are safe to repeat.
async fn queued(
    gh: &Installation,
    repo: &RepoName,
    pr: u64,
    label: &str,
    login: &str,
) -> Result<(), ApiError> {
    gh.add_label(repo, pr, label)
        .await
        .map_err(|e| refused(e, &format!("GitHub would not label PR #{pr} `{label}`")))?;
    gh.assign(repo, pr, login)
        .await
        .map_err(|e| refused(e, &format!("GitHub would not assign PR #{pr} to {login}")))
}

/// GitHub refused a step of opening the PR, which the next call tries again.
fn refused(e: anyhow::Error, what: &str) -> ApiError {
    match ApiError::from(e) {
        ApiError::Internal(e) => ApiError::Status(
            StatusCode::BAD_GATEWAY,
            format!("{what}: {e:#}. Call `open_build_pr` again."),
        ),
        limited => limited,
    }
}

/// A new PR from the build's branch, its body after the issue's closing line, or the one the App
/// already opened from it: GitHub refuses a second, and one opened by a call whose answer was
/// lost, or by a racing call, is found by its branch. Any other PR from that branch is not the
/// build's. Whether this call created it.
async fn opened(
    app: &AppState,
    gh: &Installation,
    build: &Build,
    base: &str,
    title: &str,
    body: &str,
) -> Result<(u64, bool), ApiError> {
    let (repo, head) = (&build.repo, branch(build.issue));
    // The server's line goes first, so no Markdown the caller's text opens can swallow it. The
    // caller's own mentions and closing words stay: they may push to the repo, and could write
    // the same in a PR of their own.
    let body = format!("Closes #{}\n\n{}", build.issue, body.trim());
    let e = match gh.open_pr(repo, &head, base, title, body.trim_end()).await {
        Ok(pr) => return Ok((pr, true)),
        Err(e) => e,
    };
    let bot = bot(app).await?;
    let found = gh.pulls_from(repo, &head).await?;
    if let Some(pr) = found
        .iter()
        .find(|pr| not_the_builds(pr, repo, &head, base, &bot).is_none())
    {
        return Ok((pr.number, false));
    }
    let message = match found.first() {
        Some(pr) => format!(
            "GitHub would not open a PR from {head} into {base}, and PR #{} from that branch {}, so it is not this build's. Close it, or give the build back.",
            pr.number,
            not_the_builds(pr, repo, &head, base, &bot).unwrap_or_default()
        ),
        None => format!(
            "GitHub would not open a PR from {head} into {base}. Is the branch pushed? {e:#}"
        ),
    };
    Err(ApiError::Status(StatusCode::CONFLICT, message))
}

/// Why `pr` is not the PR the App opened for a build from `head` into `base`, if it is not.
fn not_the_builds(
    pr: &LanePull,
    repo: &RepoName,
    head: &str,
    base: &str,
    bot: &str,
) -> Option<String> {
    if pr.head_repo.as_deref() != Some(&*repo.to_string()) || pr.head != head {
        Some(format!("is from another repo's {}", pr.head))
    } else if pr.base != base {
        Some(format!("goes into {}", pr.base))
    } else if !pr.open {
        Some("is closed".into())
    } else if pr.author != bot {
        Some(format!("was opened by {}, not the App", pr.author))
    } else {
        None
    }
}

#[derive(Clone, Copy, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum StandingView {
    /// You act on what the queue needs of the PR's author.
    Landing,
    /// The PR waits on a human.
    Waiting,
    /// The PR merged: the build is over.
    Merged,
}

impl From<StandingView> for Standing {
    fn from(s: StandingView) -> Self {
        match s {
            StandingView::Landing => Self::Landing,
            StandingView::Waiting => Self::Waiting,
            StandingView::Merged => Self::Merged,
        }
    }
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct ReportBuild {
    pub claim: u64,
    /// The daemon that claimed it, as `claim_build` was told.
    #[serde(default)]
    #[ts(optional)]
    pub daemon: Option<String>,
    pub standing: StandingView,
}

/// The holder says how the build's PR stands. Merged only once GitHub says so.
pub(crate) async fn report_build(
    State(app): State<AppState>,
    caller: Caller,
    Path((id, build)): Path<(u64, u64)>,
    Json(body): Json<ReportBuild>,
) -> Result<Json<BuildView>, ApiError> {
    let (id, build) = held(&app, &caller, id, build, body.daemon.as_deref(), body.claim).await?;
    if build.pr.is_none() {
        return Err(ApiError::Status(
            StatusCode::CONFLICT,
            format!(
                "Build {} has no PR yet: open it with `open_build_pr` first.",
                build.id
            ),
        ));
    }
    let standing = Standing::from(body.standing);
    if let (Standing::Merged, Some(pr)) = (standing, build.pr)
        && !installation(&app, id)
            .await?
            .lane_pull(&build.repo, pr)
            .await?
            .merged
    {
        return Err(ApiError::Status(
            StatusCode::CONFLICT,
            format!("GitHub says PR #{pr} is not merged. Report `landing` or `waiting`."),
        ));
    }
    let login = caller.user()?.login();
    let held = holder(id, build.id, login, body.daemon.as_deref(), body.claim);
    let reported = app
        .builds
        .report_build(held, standing)
        .await?
        .ok_or_else(|| lost(build.id))?;
    tracing::info!(installation = id, repo = %reported.repo, issue = reported.issue, login, status = reported.status.as_str(), "reported a build");
    Ok(Json(reported.into()))
}

/// An `issues` delivery: keeps the issue's build in step with it, while the lane is on and the
/// repo's queue runs. The label put on again makes a stuck build a task again.
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
    let label = task_label(app, id, repo).await?;
    let Some(label) = label.filter(|l| !gone && issue.labeled(l)) else {
        return Ok(app.builds.drop_task(id, repo, issue.number).await?);
    };
    let number = issue.number;
    app.builds.upsert_task(id, repo, &task(issue)).await?;
    let relabeled = payload["action"] == "labeled"
        && payload["label"]["name"]
            .as_str()
            .is_some_and(|l| l.eq_ignore_ascii_case(&label));
    if relabeled {
        app.builds.relabeled(id, repo, number).await?;
    }
    Ok(())
}

/// The label that makes the repo's open issues tasks, while its queue runs.
async fn task_label(app: &AppState, id: u64, repo: &RepoName) -> Result<Option<String>, ApiError> {
    if !app
        .store
        .repo_state(id, repo)
        .await?
        .is_some_and(|r| r.on())
    {
        return Ok(None);
    }
    let gh = installation(app, id).await?;
    Ok(Some(settings(app, &gh, id, repo).await?.0.label))
}
