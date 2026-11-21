//! The build lane's tasks: one per labeled open issue of a workspace with the lane on, which a
//! developer's daemon claims, builds, opens a PR for and lands, or gives back or marks stuck.
//!
//! A build records whether its issue is a task, and webhooks and syncs change that record: they
//! never delete a held or stuck build, so it keeps its holder and its PR. A sync deletes only
//! a ready or merged build that stopped being a task more than a day before. The intake writes of a repo
//! (a webhook's change, a sync's reconciliation) run one at a time under the repo's lock, each
//! with a new generation it stamps on the rows it writes; a sync's reconciliation writes only
//! rows no newer than the generation it noted before it read GitHub's list.

use anyhow::{Context, Result};
use async_trait::async_trait;
use mergepatrol_core::RepoName;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::PgStore;

/// A workspace's build lane settings. Never saved: the lane is off.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSettings {
    pub enabled: bool,
}

/// Where a build is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStatus {
    /// A task: anyone who may push to the repo may claim it, as its assignees allow.
    Ready,
    /// A daemon holds it, while its heartbeat is fresh, and builds it.
    Claimed,
    /// A daemon holds it, and lands its PR.
    Landing,
    /// A daemon holds it, and its PR waits on a human.
    Waiting,
    /// Its holder gave up, and said why. A human puts the label on again to make it a task
    /// again, with its PR if it has one.
    Stuck,
    /// Its PR merged.
    Merged,
}

impl BuildStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Landing => "landing",
            Self::Waiting => "waiting",
            Self::Stuck => "stuck",
            Self::Merged => "merged",
        }
    }

    /// A daemon holds it: its holder heartbeats it, and anyone may take it once that stops.
    pub fn held(self) -> bool {
        matches!(self, Self::Claimed | Self::Landing | Self::Waiting)
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "ready" => Self::Ready,
            "claimed" => Self::Claimed,
            "landing" => Self::Landing,
            "waiting" => Self::Waiting,
            "stuck" => Self::Stuck,
            "merged" => Self::Merged,
            _ => anyhow::bail!("unknown build status {s:?}"),
        })
    }
}

/// How the PR of a held build stands, as its holder reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Being landed: its holder acts on what the queue needs of its author.
    Landing,
    /// Waiting on a human.
    Waiting,
    /// Merged: the build is over.
    Merged,
}

impl Standing {
    fn status(self) -> BuildStatus {
        match self {
            Self::Landing => BuildStatus::Landing,
            Self::Waiting => BuildStatus::Waiting,
            Self::Merged => BuildStatus::Merged,
        }
    }
}

/// What the holder of a claim said it is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Building,
    Reviewing,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Reviewing => "reviewing",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "building" => Some(Self::Building),
            "reviewing" => Some(Self::Reviewing),
            _ => None,
        }
    }
}

/// An issue as GitHub has it, for its task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueTask {
    pub number: u64,
    pub title: String,
    pub body: String,
    /// Logins.
    pub assignees: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub id: u64,
    pub installation_id: u64,
    pub repo: RepoName,
    pub issue: u64,
    pub title: String,
    pub body: String,
    pub assignees: Vec<String>,
    /// Its issue is a task: open, with the repo's label.
    pub task: bool,
    pub status: BuildStatus,
    /// Who holds the claim, or held it last.
    pub holder: Option<String>,
    /// Which of `holder`'s daemons, when a daemon claimed it.
    pub daemon: Option<String>,
    /// The claim assigned `holder` on the issue.
    pub mirrored: bool,
    /// Which taking of the build `holder` has.
    pub claim: u64,
    pub stage: Option<Stage>,
    /// Seconds since the holder was last heard from.
    pub heard_secs: Option<u64>,
    pub pr: Option<u64>,
    /// Why it is stuck.
    pub reason: Option<String>,
}

impl Build {
    /// Its holder may go on with it: its issue is still a task, or it has a PR, which lands
    /// whatever its issue does.
    pub fn live(&self) -> bool {
        self.task || self.pr.is_some()
    }
}

/// The repo's generation when a sync of it began: it writes only to builds that no webhook or
/// sync wrote since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStart(i64);

/// A build just claimed, and who held it before, when a stale claim was taken over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    pub build: Build,
    /// The holder of the stale claim, and whether that claim had assigned them.
    pub before: Option<(String, bool)>,
}

/// A call from the holder of a claim: which build, who, from which of their daemons, and which
/// taking.
#[derive(Clone, Copy, Debug)]
pub struct Holder<'a> {
    pub installation_id: u64,
    pub build: u64,
    pub login: &'a str,
    /// `None` for a caller that is no daemon.
    pub daemon: Option<&'a str>,
    pub claim: u64,
}

#[async_trait]
pub trait BuildStore: Send + Sync {
    async fn build_settings(&self, installation_id: u64) -> Result<Option<BuildSettings>>;
    /// Turning the lane off drops the workspace's ready and merged builds, which a sync finds
    /// again or need not: held and stuck builds keep their rows, so turning it on loses no
    /// record.
    async fn set_build_settings(
        &self,
        installation_id: u64,
        settings: &BuildSettings,
    ) -> Result<()>;
    /// The issue is a task: adds it, or updates its title, body and assignees. A held or stuck
    /// build stays so.
    async fn upsert_task(
        &self,
        installation_id: u64,
        repo: &RepoName,
        issue: &IssueTask,
    ) -> Result<()>;
    /// The issue's label was put on again: a stuck build of it is a task again, with its PR if
    /// it has one, for whoever claims it next.
    async fn relabeled(&self, installation_id: u64, repo: &RepoName, issue: u64) -> Result<()>;
    /// The issue is no task now. Its build stays, with its holder and PR: it is claimed no more,
    /// and its holder may go on only if it has a PR.
    async fn drop_task(&self, installation_id: u64, repo: &RepoName, issue: u64) -> Result<()>;
    /// `issues` are the repo's tasks as GitHub listed them after `since`: upserts each, and,
    /// when the list is `complete`, makes every other build no task, and stamps it as this
    /// sync's. Leaves be every build a webhook or sync wrote after `since`, so an older list never
    /// undoes a newer write.
    async fn sync_tasks(
        &self,
        installation_id: u64,
        repo: &RepoName,
        since: &SyncStart,
        issues: &[IssueTask],
        complete: bool,
    ) -> Result<()>;
    /// Whether to read the repo's labeled issues now: they were last read over `every_secs`
    /// ago, or never. A yes marks them read, so one caller of many reads them, and notes the
    /// repo's generation, taken under its lock, so no intake write is half done.
    async fn take_sync(
        &self,
        installation_id: u64,
        repo: &RepoName,
        every_secs: u64,
    ) -> Result<Option<SyncStart>>;
    /// The stuck build `build`, stuck with `claim`, is a task again if its issue is one and its
    /// label was put on (`labeled_at`, RFC 3339) in the second it got stuck or later: GitHub
    /// gives whole seconds. Whether it is.
    async fn relabeled_since_stuck(
        &self,
        installation_id: u64,
        build: u64,
        claim: u64,
        labeled_at: &str,
    ) -> Result<bool>;
    /// Up to `limit` of the repo's stuck builds whose issue is a task, those checked for their
    /// label coming back longest ago first, marked checked now.
    async fn stuck_to_check(
        &self,
        installation_id: u64,
        repo: &RepoName,
        limit: u64,
    ) -> Result<Vec<Build>>;
    /// Ends each held claim in the repo on a build without a PR whose issue is no task, whose
    /// holder was not heard from for `stale_secs`: the build is ready, out of the pool, and the
    /// claim's assignee is taken off. The builds, with the holder and whether the claim had
    /// assigned them, for GitHub. A live holder's build is never touched.
    async fn retire_stale(
        &self,
        installation_id: u64,
        repo: &RepoName,
        stale_secs: u64,
    ) -> Result<Vec<Build>>;
    /// The repos with a build that some caller could claim, as `claim_build` chooses them.
    async fn claimable_repos(&self, installation_id: u64, stale_secs: u64)
    -> Result<Vec<RepoName>>;
    /// Gives `login` one build in `repos`, atomically: a ready one assigned to them, else a
    /// ready one assigned to no one, else a held one whose holder was last heard from over
    /// `stale_secs` ago or gave it back, which keeps its status and PR. Never a ready one
    /// assigned to someone else. Taking back one's own keeps what its claim assigned.
    ///
    /// Work without a PR is given only while its issue is a task. A held build with a PR is
    /// given whether or not its issue still is: its PR lands whatever the issue does, and
    /// someone must land it.
    async fn claim_build(
        &self,
        installation_id: u64,
        login: &str,
        daemon: Option<&str>,
        repos: &[RepoName],
        stale_secs: u64,
    ) -> Result<Option<Claimed>>;
    /// The holder is alive, and maybe says its stage. `None` when it holds the claim no more.
    async fn heartbeat_build(
        &self,
        holder: Holder<'_>,
        stage: Option<Stage>,
    ) -> Result<Option<Build>>;
    /// The holder gives the build back, or marks it stuck with a reason, and holds it no more.
    /// `None` when it held the claim no more. Giving back a build without a PR makes it ready,
    /// and takes away the assignee the claim added; giving back one with a PR leaves it as it
    /// is, for anyone to take at once. When its PR is `closed_pr`, closed without merging, the
    /// build forgets it: put back, the issue is a fresh task.
    async fn release_build(
        &self,
        holder: Holder<'_>,
        stuck: Option<&str>,
        closed_pr: Option<u64>,
    ) -> Result<Option<Build>>;
    /// The build, while `holder` holds it.
    async fn held_build(&self, holder: Holder<'_>) -> Result<Option<Build>>;
    /// Whether `holder` holds a live build, in a workspace whose lane is on, all read at once.
    async fn holds(&self, holder: Holder<'_>) -> Result<bool>;
    /// The holder opened PR `pr` for the build: it is landing. `None` when it holds the claim
    /// no more, the build has another PR, or `pr` is being closed as an orphan.
    async fn opened_pr(&self, holder: Holder<'_>, pr: u64) -> Result<Option<Build>>;
    /// PR `pr` was opened for the build and its opener could not record it. True when no holder
    /// recorded it either: it is to be closed, and none adopts it from now on. False when a
    /// holder did, so it is theirs and stays open. One statement decides, against
    /// [`Self::opened_pr`] on the same row.
    async fn orphaned_pr(&self, installation_id: u64, build: u64, pr: u64) -> Result<bool>;
    /// The holder says how the build's PR stands. `None` when it holds the claim no more, or
    /// the build has no PR.
    async fn report_build(&self, holder: Holder<'_>, standing: Standing) -> Result<Option<Build>>;
    /// The workspace's builds, oldest first.
    async fn builds(&self, installation_id: u64) -> Result<Vec<Build>>;
}

/// What every read of a build selects.
macro_rules! columns {
    () => {
        "id, installation_id, repo, issue, title, body, assignees, task, status, holder, daemon, mirrored, claim,
         stage, pr, reason, EXTRACT(EPOCH FROM now() - heartbeat_at)::BIGINT AS heard_secs"
    };
}

/// The statuses of a held build, in SQL.
macro_rules! held {
    () => {
        "('claimed', 'landing', 'waiting')"
    };
}

/// Matches the build `holder` holds: `$1` workspace, `$2` build, `$3` login, `$4` claim, `$5`
/// daemon.
macro_rules! fence {
    () => {
        concat!(
            " installation_id = $1 AND id = $2 AND holder = $3 AND claim = $4",
            " AND daemon IS NOT DISTINCT FROM $5 AND status IN ",
            held!()
        )
    };
}

fn fenced<'q>(
    sql: &'static str,
    holder: Holder<'q>,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    sqlx::query(sql)
        .bind(holder.installation_id as i64)
        .bind(holder.build as i64)
        .bind(holder.login)
        .bind(holder.claim as i64)
        .bind(holder.daemon)
}

fn build(r: &sqlx::postgres::PgRow) -> Result<Build> {
    let repo: String = r.get("repo");
    Ok(Build {
        id: r.get::<i64, _>("id") as u64,
        installation_id: r.get::<i64, _>("installation_id") as u64,
        repo: RepoName::parse(&repo).with_context(|| format!("build repo {repo:?}"))?,
        issue: r.get::<i64, _>("issue") as u64,
        title: r.get("title"),
        body: r.get("body"),
        assignees: r.get("assignees"),
        task: r.get("task"),
        status: BuildStatus::parse(r.get("status"))?,
        holder: r.get("holder"),
        daemon: r.get("daemon"),
        mirrored: r.get("mirrored"),
        claim: r.get::<i64, _>("claim") as u64,
        stage: r.get::<Option<&str>, _>("stage").and_then(Stage::parse),
        heard_secs: r
            .get::<Option<i64>, _>("heard_secs")
            .map(|s| s.max(0) as u64),
        pr: r.get::<Option<i64>, _>("pr").map(|n| n as u64),
        reason: r.get("reason"),
    })
}

/// Adds or updates a task, as intake write `$7`; `$8`, a sync's note, guards a sync's write.
macro_rules! upsert_task {
    ($guard:literal) => {
        concat!(
            "INSERT INTO builds (installation_id, repo, issue, title, body, assignees, generation)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (installation_id, repo, issue) DO UPDATE SET title = excluded.title,
               body = excluded.body, assignees = excluded.assignees, task = true,
               task_at = CASE WHEN builds.task THEN builds.task_at ELSE now() END,
               generation = excluded.generation, updated_at = now()",
            $guard
        )
    };
}

/// Takes the repo's intake lock until `tx` ends, and gives the generation of the write made
/// under it. Two repos whose keys hash alike only wait on each other.
async fn intake(tx: &mut sqlx::PgConnection, installation_id: u64, repo: &str) -> Result<i64> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::TEXT || '/' || $2), 29)")
        .bind(installation_id.to_string())
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    Ok(sqlx::query_scalar("SELECT nextval('build_generations')")
        .fetch_one(&mut *tx)
        .await?)
}

/// A stuck build made a task again: ready, or, with a PR, landing and free to take.
macro_rules! unstuck {
    () => {
        "UPDATE builds SET status = CASE WHEN pr IS NULL THEN 'ready' ELSE 'landing' END,
           reason = NULL, stuck_at = NULL, stage = NULL, heartbeat_at = NULL, updated_at = now()
         WHERE status = 'stuck' AND task AND installation_id = $1"
    };
}

fn upsert<'q>(
    sql: &'static str,
    installation_id: u64,
    repo: &'q str,
    issue: &'q IssueTask,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    sqlx::query(sql)
        .bind(installation_id as i64)
        .bind(repo)
        .bind(issue.number as i64)
        .bind(&issue.title)
        .bind(&issue.body)
        .bind(&issue.assignees)
}

#[async_trait]
impl BuildStore for PgStore {
    async fn build_settings(&self, installation_id: u64) -> Result<Option<BuildSettings>> {
        let row = sqlx::query("SELECT settings FROM build_settings WHERE installation_id = $1")
            .bind(installation_id as i64)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| Ok(serde_json::from_value(r.get("settings"))?))
            .transpose()
    }

    async fn set_build_settings(
        &self,
        installation_id: u64,
        settings: &BuildSettings,
    ) -> Result<()> {
        let id = installation_id as i64;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO build_settings (installation_id, settings) VALUES ($1, $2)
             ON CONFLICT (installation_id) DO UPDATE SET settings = $2, updated_at = now()",
        )
        .bind(id)
        .bind(serde_json::to_value(settings)?)
        .execute(&mut *tx)
        .await?;
        if !settings.enabled {
            for statement in [
                "DELETE FROM builds WHERE installation_id = $1 AND status IN ('ready', 'merged')",
                "DELETE FROM build_syncs WHERE installation_id = $1",
            ] {
                sqlx::query(statement).bind(id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn upsert_task(
        &self,
        installation_id: u64,
        repo: &RepoName,
        issue: &IssueTask,
    ) -> Result<()> {
        let repo = repo.to_string();
        let mut tx = self.pool.begin().await?;
        let generation = intake(&mut tx, installation_id, &repo).await?;
        upsert(upsert_task!(""), installation_id, &repo, issue)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn relabeled(&self, installation_id: u64, repo: &RepoName, issue: u64) -> Result<()> {
        sqlx::query(concat!(unstuck!(), " AND repo = $2 AND issue = $3"))
            .bind(installation_id as i64)
            .bind(repo.to_string())
            .bind(issue as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn drop_task(&self, installation_id: u64, repo: &RepoName, issue: u64) -> Result<()> {
        // A build that was never a task still records that it is none: a sync that began
        // earlier must not make it one.
        let repo = repo.to_string();
        let mut tx = self.pool.begin().await?;
        let generation = intake(&mut tx, installation_id, &repo).await?;
        sqlx::query(
            "INSERT INTO builds (installation_id, repo, issue, task, generation)
             VALUES ($1, $2, $3, false, $4)
             ON CONFLICT (installation_id, repo, issue) DO UPDATE SET task = false,
               task_at = CASE WHEN builds.task THEN now() ELSE builds.task_at END,
               generation = $4, updated_at = now()",
        )
        .bind(installation_id as i64)
        .bind(&repo)
        .bind(issue as i64)
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn sync_tasks(
        &self,
        installation_id: u64,
        repo: &RepoName,
        since: &SyncStart,
        issues: &[IssueTask],
        complete: bool,
    ) -> Result<()> {
        let repo = repo.to_string();
        let mut tx = self.pool.begin().await?;
        let generation = intake(&mut tx, installation_id, &repo).await?;
        for issue in issues {
            upsert(
                upsert_task!(" WHERE builds.generation <= $8"),
                installation_id,
                &repo,
                issue,
            )
            .bind(generation)
            .bind(since.0)
            .execute(&mut *tx)
            .await?;
        }
        if complete {
            // Every build it saw absent is this sync's, already no task or not, so an older
            // list cannot bring one back.
            let numbers: Vec<i64> = issues.iter().map(|i| i.number as i64).collect();
            sqlx::query(
                "UPDATE builds SET task = false,
                   task_at = CASE WHEN task THEN now() ELSE task_at END,
                   generation = $3, updated_at = now()
                 WHERE installation_id = $1 AND repo = $2 AND generation <= $4
                   AND NOT issue = ANY($5)",
            )
            .bind(installation_id as i64)
            .bind(&repo)
            .bind(generation)
            .bind(since.0)
            .bind(&numbers)
            .execute(&mut *tx)
            .await?;
        }
        // A day on, no sync that noted a generation before a build stopped being a task is
        // still running: its record is no longer needed.
        sqlx::query(
            "DELETE FROM builds WHERE installation_id = $1 AND repo = $2 AND NOT task
               AND status IN ('ready', 'merged') AND task_at < now() - interval '1 day'",
        )
        .bind(installation_id as i64)
        .bind(&repo)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn take_sync(
        &self,
        installation_id: u64,
        repo: &RepoName,
        every_secs: u64,
    ) -> Result<Option<SyncStart>> {
        let repo = repo.to_string();
        let mut tx = self.pool.begin().await?;
        let generation = intake(&mut tx, installation_id, &repo).await?;
        let taken: Option<i32> = sqlx::query_scalar(
            "INSERT INTO build_syncs (installation_id, repo) VALUES ($1, $2)
             ON CONFLICT (installation_id, repo) DO UPDATE SET synced_at = now()
             WHERE build_syncs.synced_at < now() - make_interval(secs => $3)
             RETURNING 1",
        )
        .bind(installation_id as i64)
        .bind(&repo)
        .bind(every_secs as f64)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(taken.map(|_| SyncStart(generation)))
    }

    async fn relabeled_since_stuck(
        &self,
        installation_id: u64,
        build: u64,
        claim: u64,
        labeled_at: &str,
    ) -> Result<bool> {
        let done = sqlx::query(concat!(
            unstuck!(),
            " AND id = $2 AND claim = $3
              AND $4::TIMESTAMPTZ >= date_trunc('second', stuck_at)"
        ))
        .bind(installation_id as i64)
        .bind(build as i64)
        .bind(claim as i64)
        .bind(labeled_at)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn stuck_to_check(
        &self,
        installation_id: u64,
        repo: &RepoName,
        limit: u64,
    ) -> Result<Vec<Build>> {
        let rows = sqlx::query(concat!(
            "UPDATE builds SET stuck_checked_at = now() WHERE id IN (
               SELECT id FROM builds
               WHERE installation_id = $1 AND repo = $2 AND status = 'stuck' AND task
               ORDER BY stuck_checked_at NULLS FIRST, id LIMIT $3 FOR UPDATE SKIP LOCKED)
             RETURNING ",
            columns!()
        ))
        .bind(installation_id as i64)
        .bind(repo.to_string())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut builds: Vec<Build> = rows.iter().map(build).collect::<Result<_>>()?;
        builds.sort_by_key(|b| b.id);
        Ok(builds)
    }

    async fn retire_stale(
        &self,
        installation_id: u64,
        repo: &RepoName,
        stale_secs: u64,
    ) -> Result<Vec<Build>> {
        let rows = sqlx::query(concat!(
            "UPDATE builds SET status = 'ready', claim = claim + 1,
               assignees = CASE WHEN mirrored THEN array_remove(assignees, holder) ELSE assignees END,
               stage = NULL, heartbeat_at = NULL, updated_at = now()
             WHERE installation_id = $1 AND repo = $2 AND NOT task AND pr IS NULL
               AND status IN ",
            held!(),
            " AND (heartbeat_at IS NULL OR heartbeat_at < now() - make_interval(secs => $3))
             RETURNING ",
            columns!()
        ))
        .bind(installation_id as i64)
        .bind(repo.to_string())
        .bind(stale_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(build).collect()
    }

    async fn claimable_repos(
        &self,
        installation_id: u64,
        stale_secs: u64,
    ) -> Result<Vec<RepoName>> {
        let rows = sqlx::query(concat!(
            "SELECT DISTINCT repo FROM builds WHERE installation_id = $1 AND (
               (status = 'ready' AND task)
               OR (status IN ",
            held!(),
            " AND (task OR pr IS NOT NULL) AND (heartbeat_at IS NULL
                 OR heartbeat_at < now() - make_interval(secs => $2)))) ORDER BY repo"
        ))
        .bind(installation_id as i64)
        .bind(stale_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| RepoName::parse(r.get("repo")))
            .collect())
    }

    async fn claim_build(
        &self,
        installation_id: u64,
        login: &str,
        daemon: Option<&str>,
        repos: &[RepoName],
        stale_secs: u64,
    ) -> Result<Option<Claimed>> {
        if repos.is_empty() {
            return Ok(None);
        }
        let repos: Vec<String> = repos.iter().map(RepoName::to_string).collect();
        let row = sqlx::query(concat!(
            "WITH pick AS (
               SELECT id AS pick_id, status AS old_status, holder AS old_holder, mirrored AS old_mirrored
               FROM builds
               WHERE installation_id = $1 AND repo = ANY($3) AND (
                 (status = 'ready' AND task AND (assignees = '{}' OR $2 = ANY(assignees)))
                 OR (status IN ", held!(), " AND (task OR pr IS NOT NULL) AND (heartbeat_at IS NULL
                   OR heartbeat_at < now() - make_interval(secs => $4))))
               ORDER BY status = 'ready' DESC, $2 = ANY(assignees) DESC, id
               LIMIT 1 FOR UPDATE SKIP LOCKED
             )
             UPDATE builds b SET
               status = CASE WHEN b.status = 'ready' THEN 'claimed' ELSE b.status END, holder = $2, daemon = $5,
               claim = b.claim + 1,
               mirrored = CASE WHEN b.status IN ", held!(), " AND b.holder = $2 THEN b.mirrored
                 ELSE NOT ($2 = ANY(b.assignees)) END,
               stage = NULL, heartbeat_at = now(),
               updated_at = now()
             FROM pick WHERE b.id = pick.pick_id
             RETURNING ", columns!(), ", pick.old_status, pick.old_holder, pick.old_mirrored"
        ))
        .bind(installation_id as i64)
        .bind(login)
        .bind(&repos)
        .bind(stale_secs as f64)
        .bind(daemon)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else {
            return Ok(None);
        };
        let held = BuildStatus::parse(r.get("old_status"))?.held();
        let before = match r.get("old_holder") {
            Some(holder) if held => Some((holder, r.get("old_mirrored"))),
            _ => None,
        };
        Ok(Some(Claimed {
            build: build(&r)?,
            before,
        }))
    }

    async fn heartbeat_build(
        &self,
        holder: Holder<'_>,
        stage: Option<Stage>,
    ) -> Result<Option<Build>> {
        let row = fenced(
            concat!(
                "UPDATE builds SET heartbeat_at = now(), stage = COALESCE($6, stage),
                   updated_at = now()
                 WHERE",
                fence!(),
                " RETURNING ",
                columns!()
            ),
            holder,
        )
        .bind(stage.map(Stage::as_str))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(build).transpose()
    }

    async fn release_build(
        &self,
        holder: Holder<'_>,
        stuck: Option<&str>,
        closed_pr: Option<u64>,
    ) -> Result<Option<Build>> {
        // The right-hand sides read the row as it was before the update.
        let row = fenced(
            concat!(
                "UPDATE builds SET
                   status = CASE WHEN $6::TEXT IS NOT NULL THEN 'stuck'
                     WHEN pr IS NULL THEN 'ready' ELSE status END,
                   reason = $6, stuck_at = CASE WHEN $6::TEXT IS NOT NULL THEN now() END,
                   claim = claim + 1,
                   assignees = CASE WHEN $6::TEXT IS NULL AND pr IS NULL AND mirrored
                     THEN array_remove(assignees, holder) ELSE assignees END,
                   pr = CASE WHEN pr = $7 THEN NULL ELSE pr END,
                   stage = NULL, heartbeat_at = NULL, updated_at = now()
                 WHERE",
                fence!(),
                " RETURNING ",
                columns!()
            ),
            holder,
        )
        .bind(stuck)
        .bind(closed_pr.map(|pr| pr as i64))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(build).transpose()
    }

    async fn held_build(&self, holder: Holder<'_>) -> Result<Option<Build>> {
        let row = fenced(
            concat!("SELECT ", columns!(), " FROM builds WHERE", fence!()),
            holder,
        )
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(build).transpose()
    }

    async fn holds(&self, holder: Holder<'_>) -> Result<bool> {
        let held: bool = sqlx::query_scalar(concat!(
            "SELECT EXISTS (SELECT 1 FROM builds WHERE",
            fence!(),
            " AND (task OR pr IS NOT NULL) AND EXISTS (SELECT 1 FROM build_settings s
                WHERE s.installation_id = $1 AND (s.settings->>'enabled')::BOOLEAN))"
        ))
        .bind(holder.installation_id as i64)
        .bind(holder.build as i64)
        .bind(holder.login)
        .bind(holder.claim as i64)
        .bind(holder.daemon)
        .fetch_one(&self.pool)
        .await?;
        Ok(held)
    }

    async fn opened_pr(&self, holder: Holder<'_>, pr: u64) -> Result<Option<Build>> {
        let row = fenced(
            concat!(
                "UPDATE builds SET pr = $6,
                   status = CASE WHEN status = 'claimed' THEN 'landing' ELSE status END,
                   updated_at = now()
                 WHERE",
                fence!(),
                " AND (pr IS NULL OR pr = $6) AND orphan_pr IS DISTINCT FROM $6 RETURNING ",
                columns!()
            ),
            holder,
        )
        .bind(pr as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(build).transpose()
    }

    async fn orphaned_pr(&self, installation_id: u64, build: u64, pr: u64) -> Result<bool> {
        // A build whose row is gone records nothing, so its PR is an orphan too.
        let orphan: bool = sqlx::query_scalar(
            "WITH mark AS (
               UPDATE builds SET orphan_pr = $3
               WHERE installation_id = $1 AND id = $2 AND pr IS DISTINCT FROM $3 RETURNING 1)
             SELECT EXISTS (SELECT 1 FROM mark)
                 OR NOT EXISTS (SELECT 1 FROM builds WHERE installation_id = $1 AND id = $2)",
        )
        .bind(installation_id as i64)
        .bind(build as i64)
        .bind(pr as i64)
        .fetch_one(&self.pool)
        .await?;
        Ok(orphan)
    }

    async fn report_build(&self, holder: Holder<'_>, standing: Standing) -> Result<Option<Build>> {
        let row = fenced(
            concat!(
                "UPDATE builds SET status = $6,
                   heartbeat_at = CASE WHEN $6 = 'merged' THEN NULL ELSE heartbeat_at END,
                   stage = NULL, updated_at = now()
                 WHERE",
                fence!(),
                " AND pr IS NOT NULL RETURNING ",
                columns!()
            ),
            holder,
        )
        .bind(standing.status().as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(build).transpose()
    }

    async fn builds(&self, installation_id: u64) -> Result<Vec<Build>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM builds WHERE installation_id = $1 ORDER BY id"
        ))
        .bind(installation_id as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(build).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;

    use super::*;
    use crate::Store;

    const STALE: u64 = 1800;

    fn repo(s: &str) -> RepoName {
        RepoName::parse(s).unwrap()
    }

    fn issue(number: u64, assignees: &[&str]) -> IssueTask {
        IssueTask {
            number,
            title: format!("Issue {number}"),
            body: "Do it.".into(),
            assignees: assignees.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    async fn store(pool: PgPool) -> PgStore {
        PgStore::migrate(pool).await.unwrap()
    }

    async fn claim(store: &PgStore, login: &str) -> Option<Build> {
        store
            .claim_build(1, login, None, &[repo("o/r")], STALE)
            .await
            .unwrap()
            .map(|c| c.build)
    }

    fn held<'a>(b: &'a Build, login: &'a str) -> Holder<'a> {
        Holder {
            installation_id: 1,
            build: b.id,
            login,
            daemon: b.daemon.as_deref(),
            claim: b.claim,
        }
    }

    /// A complete sync of `r` that lists `issues`.
    async fn sync(store: &PgStore, r: &RepoName, issues: &[IssueTask]) {
        let since = store.take_sync(1, r, 0).await.unwrap().unwrap();
        store.sync_tasks(1, r, &since, issues, true).await.unwrap();
    }

    /// As if the holder were last heard from `secs` ago.
    async fn heard_ago(pool: &PgPool, id: u64, secs: i64) {
        sqlx::query(
            "UPDATE builds SET heartbeat_at = now() - make_interval(secs => $2) WHERE id = $1",
        )
        .bind(id as i64)
        .bind(secs as f64)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn the_lane_is_off_until_turned_on_and_off_drops_its_builds(pool: PgPool) {
        let store = store(pool).await;
        assert_eq!(store.build_settings(1).await.unwrap(), None);
        let on = BuildSettings { enabled: true };
        store.set_build_settings(1, &on).await.unwrap();
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        store
            .upsert_task(2, &repo("x/y"), &issue(1, &[]))
            .await
            .unwrap();
        assert_eq!(store.build_settings(1).await.unwrap(), Some(on));

        store
            .set_build_settings(1, &BuildSettings::default())
            .await
            .unwrap();
        assert!(store.builds(1).await.unwrap().is_empty());
        assert_eq!(
            store.builds(2).await.unwrap().len(),
            1,
            "another workspace's"
        );
    }

    /// Each claim runs on its own thread, runtime and connection, and all start at once, so
    /// they really race in Postgres.
    #[sqlx::test(migrations = false)]
    async fn many_concurrent_claims_get_distinct_builds_and_the_rest_nothing(pool: PgPool) {
        const TASKS: u64 = 10;
        const CLAIMS: usize = 40;
        let store = store(pool.clone()).await;
        for n in 1..=TASKS {
            store
                .upsert_task(1, &repo("o/r"), &issue(n, &[]))
                .await
                .unwrap();
        }
        let options = pool.connect_options();
        let start = Arc::new(std::sync::Barrier::new(CLAIMS));
        let claims: Vec<_> = (0..CLAIMS)
            .map(|i| {
                let (options, start) = (options.clone(), start.clone());
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async {
                        let pool = PgPool::connect_with((*options).clone()).await.unwrap();
                        let store = PgStore { pool };
                        start.wait();
                        claim(&store, &format!("dev{i}")).await.map(|b| b.issue)
                    })
                })
            })
            .collect();
        let mut got: Vec<u64> = claims
            .into_iter()
            .filter_map(|c| c.join().unwrap())
            .collect();
        got.sort();
        assert_eq!(got, (1..=TASKS).collect::<Vec<_>>());
    }

    #[sqlx::test(migrations = false)]
    async fn a_task_assigned_to_ana_goes_to_ana_first_and_never_to_ben(pool: PgPool) {
        let store = store(pool).await;
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        store
            .upsert_task(1, &repo("o/r"), &issue(2, &["ana"]))
            .await
            .unwrap();
        store
            .upsert_task(1, &repo("o/r"), &issue(3, &["cy"]))
            .await
            .unwrap();

        let ana = claim(&store, "ana").await.unwrap();
        assert_eq!(
            (ana.issue, ana.mirrored),
            (2, false),
            "hers first, already hers"
        );
        let ben = claim(&store, "ben").await.unwrap();
        assert_eq!((ben.issue, ben.mirrored), (1, true));
        assert_eq!(claim(&store, "ben").await, None, "#3 is Cy's");
    }

    #[sqlx::test(migrations = false)]
    async fn only_the_given_repos_are_claimed(pool: PgPool) {
        let store = store(pool).await;
        store
            .upsert_task(1, &repo("o/secret"), &issue(1, &[]))
            .await
            .unwrap();
        assert_eq!(claim(&store, "ana").await, None);
        let none = store.claim_build(1, "ana", None, &[], STALE).await.unwrap();
        assert_eq!(none, None);
    }

    #[sqlx::test(migrations = false)]
    async fn another_daemon_of_the_same_login_is_another_holder(pool: PgPool) {
        let store = store(pool.clone()).await;
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        let first = store
            .claim_build(1, "ana", Some("laptop"), &[repo("o/r")], STALE)
            .await
            .unwrap()
            .unwrap()
            .build;
        assert_eq!(first.daemon.as_deref(), Some("laptop"));
        let other = Holder {
            daemon: Some("desktop"),
            ..held(&first, "ana")
        };
        assert_eq!(store.heartbeat_build(other, None).await.unwrap(), None);
        assert_eq!(store.release_build(other, None, None).await.unwrap(), None);
        let no_daemon = Holder {
            daemon: None,
            ..held(&first, "ana")
        };
        assert!(!store.holds(no_daemon).await.unwrap());
        let fresh = store
            .claim_build(1, "ana", Some("desktop"), &[repo("o/r")], STALE)
            .await
            .unwrap();
        assert_eq!(fresh, None, "a live claim is the first daemon's");

        heard_ago(&pool, first.id, STALE as i64 + 1).await;
        let taken = store
            .claim_build(1, "ana", Some("desktop"), &[repo("o/r")], STALE)
            .await
            .unwrap()
            .unwrap()
            .build;
        assert_eq!(taken.daemon.as_deref(), Some("desktop"));
        let old = held(&first, "ana");
        assert_eq!(store.heartbeat_build(old, None).await.unwrap(), None);
        assert!(
            store
                .heartbeat_build(held(&taken, "ana"), None)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test(migrations = false)]
    async fn a_stale_claim_is_taken_over_and_fences_out_its_old_holder(pool: PgPool) {
        let store = store(pool.clone()).await;
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &["ana"]))
            .await
            .unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        assert_eq!(claim(&store, "ben").await, None, "fresh: Ana's");

        heard_ago(&pool, ana.id, STALE as i64 + 1).await;
        assert_eq!(
            store.claimable_repos(1, STALE).await.unwrap(),
            [repo("o/r")]
        );
        let ben = store
            .claim_build(1, "ben", None, &[repo("o/r")], STALE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ben.before, Some(("ana".into(), false)));
        assert_eq!(ben.build.claim, ana.claim + 1);

        let late = store
            .heartbeat_build(held(&ana, "ana"), None)
            .await
            .unwrap();
        assert_eq!(late, None, "Ana's heartbeat is refused");
        let late = store
            .release_build(held(&ana, "ana"), Some("x"), None)
            .await
            .unwrap();
        assert_eq!(late, None, "and her release");
        let beat = store
            .heartbeat_build(held(&ben.build, "ben"), Some(Stage::Reviewing))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(beat.stage, Some(Stage::Reviewing));
    }

    #[sqlx::test(migrations = false)]
    async fn a_heartbeat_is_the_holders_only(pool: PgPool) {
        let store = store(pool).await;
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        let ben = Holder {
            login: "ben",
            ..held(&ana, "ana")
        };
        assert_eq!(store.heartbeat_build(ben, None).await.unwrap(), None);
        let other = Holder {
            installation_id: 2,
            ..held(&ana, "ana")
        };
        assert_eq!(store.release_build(other, None, None).await.unwrap(), None);
    }

    #[sqlx::test(migrations = false)]
    async fn giving_back_takes_away_the_assignee_the_claim_added(pool: PgPool) {
        let store = store(pool).await;
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        // The webhook tells of the claim's assignment.
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &["ana"]))
            .await
            .unwrap();
        let back = store
            .release_build(held(&ana, "ana"), None, None)
            .await
            .unwrap();
        let back = back.unwrap();
        assert_eq!((back.status, back.assignees.len()), (BuildStatus::Ready, 0));
        assert_eq!(claim(&store, "ben").await.map(|b| b.issue), Some(1));
    }

    #[sqlx::test(migrations = false)]
    async fn a_stuck_build_is_no_task_until_its_label_is_put_on_again(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        let stuck = store
            .release_build(held(&ana, "ana"), Some("tests never pass"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stuck.status, BuildStatus::Stuck);
        assert_eq!(stuck.reason.as_deref(), Some("tests never pass"));
        assert_eq!(claim(&store, "ben").await, None);
        // An edit, a sync that finds it still labeled, or the label going off, leave it stuck.
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        sync(&store, &r, &[issue(1, &[])]).await;
        store.drop_task(1, &r, 1).await.unwrap();
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        assert_eq!(claim(&store, "ben").await, None);

        store.relabeled(1, &r, 1).await.unwrap();
        assert_eq!(claim(&store, "ben").await.map(|b| b.issue), Some(1));
    }

    #[sqlx::test(migrations = false)]
    async fn a_sync_makes_the_unlisted_no_task_and_keeps_their_holders(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        for n in 1..=3 {
            store.upsert_task(1, &r, &issue(n, &[])).await.unwrap();
        }
        let held = claim(&store, "ana").await.unwrap();
        assert_eq!(held.issue, 1);

        sync(&store, &r, &[issue(3, &["ben"]), issue(4, &[])]).await;
        let left: Vec<(u64, bool, Vec<String>)> = store
            .builds(1)
            .await
            .unwrap()
            .into_iter()
            .map(|b| (b.issue, b.task, b.assignees))
            .collect();
        assert_eq!(
            left,
            [
                (1, false, vec![]),
                (2, false, vec![]),
                (3, true, vec!["ben".into()]),
                (4, true, vec![])
            ],
            "#1 and #2 left the label, and #1 is still Ana's"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn a_renamed_repo_keeps_its_builds_and_claims(pool: PgPool) {
        use crate::Store;
        let store = store(pool).await;
        let (old, new) = (repo("o/old"), repo("o/new"));
        store.upsert_task(1, &old, &issue(1, &[])).await.unwrap();
        store.upsert_task(1, &old, &issue(2, &[])).await.unwrap();
        let ana = store
            .claim_build(1, "ana", None, std::slice::from_ref(&old), STALE)
            .await
            .unwrap()
            .unwrap();
        // The webhook heard of #2 under the new name first.
        store
            .upsert_task(1, &new, &issue(2, &["ben"]))
            .await
            .unwrap();
        assert!(store.take_sync(1, &old, 300).await.unwrap().is_some());

        store.rename_repo(1, &old, &new).await.unwrap();

        let builds = store.builds(1).await.unwrap();
        let named: Vec<(String, u64, BuildStatus)> = builds
            .iter()
            .map(|b| (b.repo.to_string(), b.issue, b.status))
            .collect();
        assert_eq!(
            named,
            [
                ("o/new".into(), 1, BuildStatus::Claimed),
                ("o/new".into(), 2, BuildStatus::Ready),
            ]
        );
        let beat = store
            .heartbeat_build(held(&ana.build, "ana"), None)
            .await
            .unwrap();
        assert!(beat.is_some(), "Ana still holds #1");
        assert!(store.take_sync(1, &new, 300).await.unwrap().is_some());
    }

    /// Ana claims #1 and opens PR 40 for it.
    async fn landing(store: &PgStore) -> Build {
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        let ana = claim(store, "ana").await.unwrap();
        store
            .opened_pr(held(&ana, "ana"), 40)
            .await
            .unwrap()
            .unwrap()
    }

    #[sqlx::test(migrations = false)]
    async fn a_held_build_is_read_only_by_its_holder_with_its_claim(pool: PgPool) {
        let store = store(pool).await;
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        assert_eq!(
            store.held_build(held(&ana, "ana")).await.unwrap(),
            Some(ana.clone())
        );
        let ben = Holder {
            login: "ben",
            ..held(&ana, "ana")
        };
        assert_eq!(store.held_build(ben).await.unwrap(), None);
        let old = Holder {
            claim: ana.claim - 1,
            ..held(&ana, "ana")
        };
        assert_eq!(store.held_build(old).await.unwrap(), None);
        store
            .release_build(held(&ana, "ana"), Some("no"), None)
            .await
            .unwrap();
        assert_eq!(
            store.held_build(held(&ana, "ana")).await.unwrap(),
            None,
            "stuck"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn opening_a_pr_makes_the_build_landing_and_twice_keeps_the_first(pool: PgPool) {
        let store = store(pool).await;
        let pr = landing(&store).await;
        assert_eq!((pr.status, pr.pr), (BuildStatus::Landing, Some(40)));
        let again = store.opened_pr(held(&pr, "ana"), 40).await.unwrap();
        assert_eq!(again.map(|b| b.status), Some(BuildStatus::Landing));
        let other = store.opened_pr(held(&pr, "ana"), 41).await.unwrap();
        assert_eq!(other, None, "a build has one PR");
        let ben = Holder {
            login: "ben",
            ..held(&pr, "ana")
        };
        assert_eq!(store.opened_pr(ben, 40).await.unwrap(), None);
    }

    #[sqlx::test(migrations = false)]
    async fn a_built_pr_is_its_holders_and_passes_to_whoever_takes_it_over(pool: PgPool) {
        let store = store(pool.clone()).await;
        let pr = landing(&store).await;
        // A build with no PR yet, in the same repo, and one of another repo.
        for (r, n) in [("o/r", 2), ("o/other", 3)] {
            store
                .upsert_task(1, &repo(r), &issue(n, &[]))
                .await
                .unwrap();
        }
        claim(&store, "cy").await.unwrap();
        let r = repo("o/r");
        assert_eq!(
            store.builders(1, &r, &[40, 41]).await.unwrap(),
            [(40, "ana".to_owned())]
        );
        // Only the PRs asked about, in the workspace asked about.
        assert!(store.builders(1, &r, &[41]).await.unwrap().is_empty());
        assert!(store.builders(2, &r, &[40]).await.unwrap().is_empty());

        heard_ago(&pool, pr.id, STALE as i64 + 1).await;
        let ben = claim(&store, "ben").await.unwrap();
        assert_eq!(ben.pr, Some(40));
        assert_eq!(
            store.builders(1, &r, &[40]).await.unwrap(),
            [(40, "ben".to_owned())]
        );
    }

    #[sqlx::test(migrations = false)]
    async fn a_landing_build_waits_lands_again_and_merges(pool: PgPool) {
        let store = store(pool).await;
        let pr = landing(&store).await;
        let report = |standing| store.report_build(held(&pr, "ana"), standing);
        assert_eq!(
            report(Standing::Waiting).await.unwrap().map(|b| b.status),
            Some(BuildStatus::Waiting)
        );
        let beat = store.heartbeat_build(held(&pr, "ana"), None).await.unwrap();
        assert!(beat.is_some(), "a waiting build is still held");
        assert_eq!(
            report(Standing::Landing).await.unwrap().map(|b| b.status),
            Some(BuildStatus::Landing)
        );
        let merged = report(Standing::Merged).await.unwrap().unwrap();
        assert_eq!(
            (merged.status, merged.pr, merged.heard_secs),
            (BuildStatus::Merged, Some(40), None)
        );
        assert_eq!(report(Standing::Waiting).await.unwrap(), None, "it is over");
        assert_eq!(store.held_build(held(&pr, "ana")).await.unwrap(), None);
    }

    #[sqlx::test(migrations = false)]
    async fn only_a_build_with_a_pr_is_reported_on(pool: PgPool) {
        let store = store(pool).await;
        store
            .upsert_task(1, &repo("o/r"), &issue(1, &[]))
            .await
            .unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        let report = store
            .report_build(held(&ana, "ana"), Standing::Merged)
            .await
            .unwrap();
        assert_eq!(report, None);
    }

    #[sqlx::test(migrations = false)]
    async fn a_stale_landing_build_is_taken_over_with_its_pr(pool: PgPool) {
        let store = store(pool.clone()).await;
        let pr = landing(&store).await;
        store
            .report_build(held(&pr, "ana"), Standing::Waiting)
            .await
            .unwrap();
        assert_eq!(claim(&store, "ben").await, None, "fresh: Ana's");
        heard_ago(&pool, pr.id, STALE as i64 + 1).await;
        let ben = claim(&store, "ben").await.unwrap();
        assert_eq!(
            (ben.status, ben.pr, ben.holder.as_deref()),
            (BuildStatus::Waiting, Some(40), Some("ben"))
        );
        let late = store.report_build(held(&pr, "ana"), Standing::Merged);
        assert_eq!(late.await.unwrap(), None, "Ana is fenced out");
    }

    #[sqlx::test(migrations = false)]
    async fn giving_back_a_build_with_a_pr_lets_anyone_take_it_at_once(pool: PgPool) {
        let store = store(pool).await;
        let pr = landing(&store).await;
        let back = store
            .release_build(held(&pr, "ana"), None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((back.status, back.pr), (BuildStatus::Landing, Some(40)));
        let ben = claim(&store, "ben").await.unwrap();
        assert_eq!((ben.status, ben.pr), (BuildStatus::Landing, Some(40)));

        let stuck = store
            .release_build(held(&ben, "ben"), Some("ejected twice"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((stuck.status, stuck.pr), (BuildStatus::Stuck, Some(40)));
    }

    #[sqlx::test(migrations = false)]
    async fn a_build_with_a_pr_lives_on_through_its_issue_going_and_the_lane_off_and_on(
        pool: PgPool,
    ) {
        let store = store(pool).await;
        let r = repo("o/r");
        let pr = landing(&store).await;
        store.drop_task(1, &r, 1).await.unwrap();
        sync(&store, &r, &[]).await;
        store
            .set_build_settings(1, &BuildSettings::default())
            .await
            .unwrap();
        store
            .set_build_settings(1, &BuildSettings { enabled: true })
            .await
            .unwrap();
        let kept = store.held_build(held(&pr, "ana")).await.unwrap();
        assert_eq!(
            kept.map(|b| (b.status, b.pr)),
            Some((BuildStatus::Landing, Some(40)))
        );

        assert!(store.holds(held(&pr, "ana")).await.unwrap(), "it has a PR");
    }

    #[sqlx::test(migrations = false)]
    async fn an_issue_that_stops_being_a_task_leaves_its_holder_only_giving_back(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        store.drop_task(1, &r, 1).await.unwrap();
        let held_on = store.held_build(held(&ana, "ana")).await.unwrap().unwrap();
        assert!(
            !held_on.live(),
            "Ana still holds it, and may only give it back"
        );
        assert!(!store.holds(held(&ana, "ana")).await.unwrap());

        let back = store
            .release_build(held(&ana, "ana"), None, None)
            .await
            .unwrap();
        assert_eq!(back.map(|b| b.status), Some(BuildStatus::Ready));
        assert_eq!(
            claim(&store, "ben").await,
            None,
            "no task, so not in the pool"
        );
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        assert_eq!(claim(&store, "ben").await.map(|b| b.issue), Some(1));
    }

    #[sqlx::test(migrations = false)]
    async fn a_stale_claim_on_an_issue_that_is_no_task_is_not_taken_over(pool: PgPool) {
        let store = store(pool.clone()).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        sync(&store, &r, &[]).await;
        heard_ago(&pool, ana.id, STALE as i64 + 1).await;
        assert_eq!(claim(&store, "ben").await, None);
        assert!(store.claimable_repos(1, STALE).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = false)]
    async fn a_sync_leaves_be_what_a_webhook_wrote_after_it_began(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let since = store.take_sync(1, &r, 0).await.unwrap().unwrap();
        // After the list was read: #1 closes, #2 gets the label, #3 never had it.
        store.drop_task(1, &r, 1).await.unwrap();
        store.upsert_task(1, &r, &issue(2, &[])).await.unwrap();
        store.drop_task(1, &r, 3).await.unwrap();
        store
            .sync_tasks(1, &r, &since, &[issue(1, &[]), issue(3, &[])], true)
            .await
            .unwrap();
        let tasks: Vec<(u64, bool)> = store
            .builds(1)
            .await
            .unwrap()
            .into_iter()
            .map(|b| (b.issue, b.task))
            .collect();
        assert_eq!(tasks, [(1, false), (2, true), (3, false)]);
    }

    #[sqlx::test(migrations = false)]
    async fn a_partial_list_makes_nothing_it_left_out_no_task(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let since = store.take_sync(1, &r, 0).await.unwrap().unwrap();
        store.sync_tasks(1, &r, &since, &[], false).await.unwrap();
        assert_eq!(claim(&store, "ana").await.map(|b| b.issue), Some(1));
    }

    #[sqlx::test(migrations = false)]
    async fn taking_back_ones_own_stale_claim_keeps_what_it_assigned(pool: PgPool) {
        let store = store(pool.clone()).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let first = claim(&store, "ana").await.unwrap();
        assert!(first.mirrored);
        store.upsert_task(1, &r, &issue(1, &["ana"])).await.unwrap();
        for status in ["claimed", "landing", "waiting"] {
            sqlx::query("UPDATE builds SET status = $1")
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
            heard_ago(&pool, first.id, STALE as i64 + 1).await;
            let again = claim(&store, "ana").await.unwrap();
            assert!(again.mirrored, "{status}");
        }
    }

    #[sqlx::test(migrations = false)]
    async fn a_stuck_build_is_a_task_again_only_if_labeled_after_it_got_stuck(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        let stuck = store
            .release_build(held(&ana, "ana"), Some("no"), None)
            .await
            .unwrap()
            .unwrap();
        let again = |at: &'static str| {
            let store = &store;
            async move {
                store
                    .relabeled_since_stuck(1, stuck.id, stuck.claim, at)
                    .await
                    .unwrap()
            }
        };
        assert!(!again("2020-01-01T00:00:00Z").await);
        assert!(again("2999-01-01T00:00:00Z").await);
        assert_eq!(claim(&store, "ben").await.map(|b| b.issue), Some(1));
    }

    #[sqlx::test(migrations = false)]
    async fn holding_is_read_with_the_lane_and_ends_with_giving_back(pool: PgPool) {
        let store = store(pool).await;
        let on = BuildSettings { enabled: true };
        store.set_build_settings(1, &on).await.unwrap();
        let pr = landing(&store).await;
        assert!(store.holds(held(&pr, "ana")).await.unwrap());
        let ben = Holder {
            login: "ben",
            ..held(&pr, "ana")
        };
        assert!(!store.holds(ben).await.unwrap());

        let back = store
            .release_build(held(&pr, "ana"), None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.claim, pr.claim + 1, "the old claim is over");
        assert!(!store.holds(held(&pr, "ana")).await.unwrap());
        let late = store.heartbeat_build(held(&pr, "ana"), None).await.unwrap();
        assert_eq!(late, None);

        let again = claim(&store, "ana").await.unwrap();
        assert!(store.holds(held(&again, "ana")).await.unwrap());
        store
            .set_build_settings(1, &BuildSettings::default())
            .await
            .unwrap();
        assert!(!store.holds(held(&again, "ana")).await.unwrap(), "lane off");
    }

    #[sqlx::test(migrations = false)]
    async fn turning_the_lane_off_drops_only_what_a_sync_finds_again(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        for n in 1..=3 {
            store.upsert_task(1, &r, &issue(n, &[])).await.unwrap();
        }
        let claimed = claim(&store, "ana").await.unwrap();
        let stuck = claim(&store, "ana").await.unwrap();
        store
            .release_build(held(&stuck, "ana"), Some("no"), None)
            .await
            .unwrap();
        store
            .set_build_settings(1, &BuildSettings::default())
            .await
            .unwrap();
        let left: Vec<(u64, BuildStatus)> = store
            .builds(1)
            .await
            .unwrap()
            .into_iter()
            .map(|b| (b.issue, b.status))
            .collect();
        assert_eq!(
            left,
            [
                (claimed.issue, BuildStatus::Claimed),
                (stuck.issue, BuildStatus::Stuck)
            ]
        );
    }

    #[sqlx::test(migrations = false)]
    async fn one_caller_of_many_syncs_a_repo(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        assert!(store.take_sync(1, &r, 300).await.unwrap().is_some());
        assert!(store.take_sync(1, &r, 300).await.unwrap().is_none());
        assert!(
            store.take_sync(1, &r, 0).await.unwrap().is_some(),
            "once it is due"
        );
    }

    fn tasks(builds: Vec<Build>) -> Vec<(u64, bool)> {
        builds.into_iter().map(|b| (b.issue, b.task)).collect()
    }

    #[sqlx::test(migrations = false)]
    async fn an_older_list_reconciled_last_leaves_be_what_a_newer_sync_saw(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        // #7 left the label long ago; then it was labeled and unlabeled, both unheard.
        store.drop_task(1, &r, 7).await.unwrap();
        let older = store.take_sync(1, &r, 0).await.unwrap().unwrap();
        let newer = store.take_sync(1, &r, 0).await.unwrap().unwrap();
        // The newer sync lists #7 absent and is reconciled first; the older listed it labeled.
        store.sync_tasks(1, &r, &newer, &[], true).await.unwrap();
        store
            .sync_tasks(1, &r, &older, &[issue(7, &[])], true)
            .await
            .unwrap();
        assert_eq!(tasks(store.builds(1).await.unwrap()), [(7, false)]);
        assert_eq!(claim(&store, "ana").await, None);
    }

    #[sqlx::test(migrations = false)]
    async fn a_repos_intake_writes_wait_on_one_another(pool: PgPool) {
        let store = store(pool.clone()).await;
        let r = repo("o/r");
        let mut first = pool.begin().await.unwrap();
        intake(&mut first, 1, "o/r").await.unwrap();

        let other = PgStore { pool: pool.clone() };
        let other_repo = tokio::spawn(async move {
            other.drop_task(1, &repo("o/y"), 1).await.unwrap();
        });
        let other = PgStore { pool: pool.clone() };
        let same_repo = tokio::spawn(async move {
            let since = other.take_sync(1, &repo("o/r"), 0).await.unwrap().unwrap();
            other
                .sync_tasks(1, &repo("o/r"), &since, &[], true)
                .await
                .unwrap();
        });
        other_repo.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !same_repo.is_finished(),
            "o/r's sync waits on the write in flight"
        );

        first.commit().await.unwrap();
        same_repo.await.unwrap();
        assert!(store.builds(1).await.unwrap().iter().all(|b| b.repo != r));
    }

    #[sqlx::test(migrations = false)]
    async fn a_build_with_a_pr_given_back_after_its_issue_closed_is_taken_by_the_next_claim(
        pool: PgPool,
    ) {
        let store = store(pool.clone()).await;
        let r = repo("o/r");
        let pr = landing(&store).await;
        store.drop_task(1, &r, 1).await.unwrap();
        store
            .release_build(held(&pr, "ana"), None, None)
            .await
            .unwrap();
        assert_eq!(store.claimable_repos(1, STALE).await.unwrap(), [r]);
        let ben = claim(&store, "ben").await.unwrap();
        assert_eq!((ben.status, ben.pr), (BuildStatus::Landing, Some(40)));

        heard_ago(&pool, ben.id, STALE as i64 + 1).await;
        let cy = claim(&store, "cy").await.unwrap();
        assert_eq!(cy.pr, Some(40), "a stale one too");
    }

    #[sqlx::test(migrations = false)]
    async fn a_label_put_back_in_the_second_the_build_got_stuck_counts(pool: PgPool) {
        let store = store(pool.clone()).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        let stuck = store
            .release_build(held(&ana, "ana"), Some("no"), None)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE builds SET stuck_at = '2026-11-12T12:00:00.800Z'")
            .execute(&pool)
            .await
            .unwrap();
        let again = |at| store.relabeled_since_stuck(1, stuck.id, stuck.claim, at);
        assert!(!again("2026-11-12T11:59:59Z").await.unwrap());
        assert!(again("2026-11-12T12:00:00Z").await.unwrap());
    }

    #[sqlx::test(migrations = false)]
    async fn stuck_builds_are_checked_a_few_at_a_time_longest_unchecked_first(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        for n in 1..=3 {
            store.upsert_task(1, &r, &issue(n, &[])).await.unwrap();
            let b = claim(&store, "ana").await.unwrap();
            store
                .release_build(held(&b, "ana"), Some("no"), None)
                .await
                .unwrap();
        }
        let checked = |limit| {
            let (store, r) = (&store, &r);
            async move {
                let builds = store.stuck_to_check(1, r, limit).await.unwrap();
                builds.into_iter().map(|b| b.issue).collect::<Vec<_>>()
            }
        };
        assert_eq!(checked(2).await.len(), 2);
        assert_eq!(checked(1).await, [3], "the one not checked yet");
        assert_eq!(checked(1).await, [1]);
    }

    #[sqlx::test(migrations = false)]
    async fn a_stale_claim_on_an_issue_that_is_no_task_is_retired_and_a_live_one_kept(
        pool: PgPool,
    ) {
        let store = store(pool.clone()).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        store.upsert_task(1, &r, &issue(1, &["ana"])).await.unwrap();
        store.drop_task(1, &r, 1).await.unwrap();
        assert!(store.retire_stale(1, &r, STALE).await.unwrap().is_empty());
        assert!(store.held_build(held(&ana, "ana")).await.unwrap().is_some());

        heard_ago(&pool, ana.id, STALE as i64 + 1).await;
        let retired = store.retire_stale(1, &r, STALE).await.unwrap();
        let shown: Vec<_> = retired
            .iter()
            .map(|b| (b.status, b.holder.as_deref(), b.mirrored, b.assignees.len()))
            .collect();
        assert_eq!(shown, [(BuildStatus::Ready, Some("ana"), true, 0)]);
        assert_eq!(store.held_build(held(&ana, "ana")).await.unwrap(), None);
        assert_eq!(claim(&store, "ben").await, None, "still no task");
    }
}
