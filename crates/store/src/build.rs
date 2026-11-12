//! The build lane's tasks: one per labeled open issue of a workspace with the lane on, which a
//! developer's daemon claims, builds, and gives back or marks stuck.

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
    /// A daemon holds it, while its heartbeat is fresh.
    Claimed,
    /// Its holder gave up, and said why. A human takes the label off and on to make it a task
    /// again.
    Stuck,
}

impl BuildStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Stuck => "stuck",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "ready" => Self::Ready,
            "claimed" => Self::Claimed,
            "stuck" => Self::Stuck,
            _ => anyhow::bail!("unknown build status {s:?}"),
        })
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
    pub status: BuildStatus,
    /// Who holds the claim, or held it last.
    pub holder: Option<String>,
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

/// A build just claimed, and who held it before, when a stale claim was taken over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    pub build: Build,
    /// The holder of the stale claim, and whether that claim had assigned them.
    pub before: Option<(String, bool)>,
}

/// A call from the holder of a claim: which build, who, and which taking.
#[derive(Clone, Copy, Debug)]
pub struct Holder<'a> {
    pub installation_id: u64,
    pub build: u64,
    pub login: &'a str,
    pub claim: u64,
}

#[async_trait]
pub trait BuildStore: Send + Sync {
    async fn build_settings(&self, installation_id: u64) -> Result<Option<BuildSettings>>;
    /// Turning the lane off drops the workspace's builds: rows are kept only while it is on.
    async fn set_build_settings(
        &self,
        installation_id: u64,
        settings: &BuildSettings,
    ) -> Result<()>;
    /// The issue is a task: adds it, or updates its title, body and assignees. A claimed or
    /// stuck build stays so.
    async fn upsert_task(
        &self,
        installation_id: u64,
        repo: &RepoName,
        issue: &IssueTask,
    ) -> Result<()>;
    /// The issue is no task now: drops its build unless it is claimed.
    async fn drop_task(&self, installation_id: u64, repo: &RepoName, issue: u64) -> Result<()>;
    /// `issues` are all the repo's tasks: upserts each, and drops the unclaimed others.
    async fn sync_tasks(
        &self,
        installation_id: u64,
        repo: &RepoName,
        issues: &[IssueTask],
    ) -> Result<()>;
    /// Whether to read the repo's labeled issues now: they were last read over `every_secs`
    /// ago, or never. A yes marks them read, so one caller of many reads them.
    async fn take_sync(
        &self,
        installation_id: u64,
        repo: &RepoName,
        every_secs: u64,
    ) -> Result<bool>;
    /// The repos with a build that some caller could claim: ready, or claimed and stale.
    async fn claimable_repos(&self, installation_id: u64, stale_secs: u64)
    -> Result<Vec<RepoName>>;
    /// Gives `login` one build in `repos`, atomically: a ready one assigned to them, else a
    /// ready one assigned to no one, else one whose claim is older than `stale_secs`. Never a
    /// ready one assigned to someone else.
    async fn claim_build(
        &self,
        installation_id: u64,
        login: &str,
        repos: &[RepoName],
        stale_secs: u64,
    ) -> Result<Option<Claimed>>;
    /// The holder is alive, and maybe says its stage. `None` when it holds the claim no more.
    async fn heartbeat_build(
        &self,
        holder: Holder<'_>,
        stage: Option<Stage>,
    ) -> Result<Option<Build>>;
    /// The holder gives the build back, or marks it stuck with a reason. `None` when it holds
    /// the claim no more. Giving back takes away the assignee the claim added.
    async fn release_build(&self, holder: Holder<'_>, stuck: Option<&str>)
    -> Result<Option<Build>>;
    /// The workspace's builds, oldest first.
    async fn builds(&self, installation_id: u64) -> Result<Vec<Build>>;
}

/// What every read of a build selects.
macro_rules! columns {
    () => {
        "id, installation_id, repo, issue, title, body, assignees, status, holder, mirrored, claim,
         stage, pr, reason, EXTRACT(EPOCH FROM now() - heartbeat_at)::BIGINT AS heard_secs"
    };
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
        status: BuildStatus::parse(r.get("status"))?,
        holder: r.get("holder"),
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

const UPSERT_TASK: &str =
    "INSERT INTO builds (installation_id, repo, issue, title, body, assignees)
     VALUES ($1, $2, $3, $4, $5, $6)
     ON CONFLICT (installation_id, repo, issue) DO UPDATE SET title = excluded.title,
        body = excluded.body, assignees = excluded.assignees, updated_at = now()";

fn upsert<'q>(
    installation_id: u64,
    repo: &'q str,
    issue: &'q IssueTask,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    sqlx::query(UPSERT_TASK)
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
                "DELETE FROM builds WHERE installation_id = $1",
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
        upsert(installation_id, &repo.to_string(), issue)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn drop_task(&self, installation_id: u64, repo: &RepoName, issue: u64) -> Result<()> {
        sqlx::query(
            "DELETE FROM builds WHERE installation_id = $1 AND repo = $2 AND issue = $3
               AND status <> 'claimed'",
        )
        .bind(installation_id as i64)
        .bind(repo.to_string())
        .bind(issue as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn sync_tasks(
        &self,
        installation_id: u64,
        repo: &RepoName,
        issues: &[IssueTask],
    ) -> Result<()> {
        let repo = repo.to_string();
        let mut tx = self.pool.begin().await?;
        for issue in issues {
            upsert(installation_id, &repo, issue)
                .execute(&mut *tx)
                .await?;
        }
        let numbers: Vec<i64> = issues.iter().map(|i| i.number as i64).collect();
        sqlx::query(
            "DELETE FROM builds WHERE installation_id = $1 AND repo = $2
               AND status <> 'claimed' AND NOT issue = ANY($3)",
        )
        .bind(installation_id as i64)
        .bind(&repo)
        .bind(&numbers)
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
    ) -> Result<bool> {
        let taken = sqlx::query(
            "INSERT INTO build_syncs (installation_id, repo) VALUES ($1, $2)
             ON CONFLICT (installation_id, repo) DO UPDATE SET synced_at = now()
             WHERE build_syncs.synced_at < now() - make_interval(secs => $3)",
        )
        .bind(installation_id as i64)
        .bind(repo.to_string())
        .bind(every_secs as f64)
        .execute(&self.pool)
        .await?;
        Ok(taken.rows_affected() == 1)
    }

    async fn claimable_repos(
        &self,
        installation_id: u64,
        stale_secs: u64,
    ) -> Result<Vec<RepoName>> {
        let rows = sqlx::query(
            "SELECT DISTINCT repo FROM builds WHERE installation_id = $1 AND (status = 'ready'
               OR (status = 'claimed' AND heartbeat_at < now() - make_interval(secs => $2)))
             ORDER BY repo",
        )
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
                 (status = 'ready' AND (assignees = '{}' OR $2 = ANY(assignees)))
                 OR (status = 'claimed' AND heartbeat_at < now() - make_interval(secs => $4)))
               ORDER BY status = 'ready' DESC, $2 = ANY(assignees) DESC, id
               LIMIT 1 FOR UPDATE SKIP LOCKED
             )
             UPDATE builds b SET status = 'claimed', holder = $2, claim = b.claim + 1,
               mirrored = NOT ($2 = ANY(b.assignees)), stage = NULL, heartbeat_at = now(),
               updated_at = now()
             FROM pick WHERE b.id = pick.pick_id
             RETURNING ", columns!(), ", pick.old_status, pick.old_holder, pick.old_mirrored"
        ))
        .bind(installation_id as i64)
        .bind(login)
        .bind(&repos)
        .bind(stale_secs as f64)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else {
            return Ok(None);
        };
        let before = match (r.get::<&str, _>("old_status"), r.get("old_holder")) {
            ("claimed", Some(holder)) => Some((holder, r.get("old_mirrored"))),
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
        let row = sqlx::query(concat!(
            "UPDATE builds SET heartbeat_at = now(), stage = COALESCE($5, stage), updated_at = now()
             WHERE installation_id = $1 AND id = $2 AND holder = $3 AND claim = $4
               AND status = 'claimed'
             RETURNING ", columns!(), ""
        ))
        .bind(holder.installation_id as i64)
        .bind(holder.build as i64)
        .bind(holder.login)
        .bind(holder.claim as i64)
        .bind(stage.map(Stage::as_str))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(build).transpose()
    }

    async fn release_build(
        &self,
        holder: Holder<'_>,
        stuck: Option<&str>,
    ) -> Result<Option<Build>> {
        // `mirrored` and `assignees` are read before the update, as the row was.
        let row = sqlx::query(concat!(
            "UPDATE builds SET
               status = CASE WHEN $5::TEXT IS NULL THEN 'ready' ELSE 'stuck' END,
               reason = $5,
               assignees = CASE WHEN $5::TEXT IS NULL AND mirrored
                 THEN array_remove(assignees, holder) ELSE assignees END,
               stage = NULL, heartbeat_at = NULL, updated_at = now()
             WHERE installation_id = $1 AND id = $2 AND holder = $3 AND claim = $4
               AND status = 'claimed'
             RETURNING ",
            columns!(),
            ""
        ))
        .bind(holder.installation_id as i64)
        .bind(holder.build as i64)
        .bind(holder.login)
        .bind(holder.claim as i64)
        .bind(stuck)
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
            .claim_build(1, login, &[repo("o/r")], STALE)
            .await
            .unwrap()
            .map(|c| c.build)
    }

    fn held<'a>(b: &'a Build, login: &'a str) -> Holder<'a> {
        Holder {
            installation_id: 1,
            build: b.id,
            login,
            claim: b.claim,
        }
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
        let none = store.claim_build(1, "ana", &[], STALE).await.unwrap();
        assert_eq!(none, None);
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
            .claim_build(1, "ben", &[repo("o/r")], STALE)
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
            .release_build(held(&ana, "ana"), Some("x"))
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
        assert_eq!(store.release_build(other, None).await.unwrap(), None);
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
        let back = store.release_build(held(&ana, "ana"), None).await.unwrap();
        let back = back.unwrap();
        assert_eq!((back.status, back.assignees.len()), (BuildStatus::Ready, 0));
        assert_eq!(claim(&store, "ben").await.map(|b| b.issue), Some(1));
    }

    #[sqlx::test(migrations = false)]
    async fn a_stuck_build_is_no_task_until_its_label_goes_off_and_on(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        let ana = claim(&store, "ana").await.unwrap();
        let stuck = store
            .release_build(held(&ana, "ana"), Some("tests never pass"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stuck.status, BuildStatus::Stuck);
        assert_eq!(stuck.reason.as_deref(), Some("tests never pass"));
        assert_eq!(claim(&store, "ben").await, None);
        // An edit, or a sync that finds it still labeled, leaves it stuck.
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        store.sync_tasks(1, &r, &[issue(1, &[])]).await.unwrap();
        assert_eq!(claim(&store, "ben").await, None);

        store.drop_task(1, &r, 1).await.unwrap();
        store.upsert_task(1, &r, &issue(1, &[])).await.unwrap();
        assert_eq!(claim(&store, "ben").await.map(|b| b.issue), Some(1));
    }

    #[sqlx::test(migrations = false)]
    async fn a_sync_keeps_the_listed_tasks_and_claims_and_drops_the_rest(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        for n in 1..=3 {
            store.upsert_task(1, &r, &issue(n, &[])).await.unwrap();
        }
        let held = claim(&store, "ana").await.unwrap();
        assert_eq!(held.issue, 1);

        store
            .sync_tasks(1, &r, &[issue(3, &["ben"]), issue(4, &[])])
            .await
            .unwrap();
        let left: Vec<(u64, Vec<String>)> = store
            .builds(1)
            .await
            .unwrap()
            .into_iter()
            .map(|b| (b.issue, b.assignees))
            .collect();
        assert_eq!(
            left,
            [(1, vec![]), (3, vec!["ben".into()]), (4, vec![])],
            "#1 is claimed, #2 left the label"
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
            .claim_build(1, "ana", std::slice::from_ref(&old), STALE)
            .await
            .unwrap()
            .unwrap();
        // The webhook heard of #2 under the new name first.
        store
            .upsert_task(1, &new, &issue(2, &["ben"]))
            .await
            .unwrap();
        assert!(store.take_sync(1, &old, 300).await.unwrap());

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
        assert!(store.take_sync(1, &new, 300).await.unwrap());
    }

    #[sqlx::test(migrations = false)]
    async fn one_caller_of_many_syncs_a_repo(pool: PgPool) {
        let store = store(pool).await;
        let r = repo("o/r");
        assert!(store.take_sync(1, &r, 300).await.unwrap());
        assert!(!store.take_sync(1, &r, 300).await.unwrap());
        assert!(store.take_sync(1, &r, 0).await.unwrap(), "once it is due");
    }
}
