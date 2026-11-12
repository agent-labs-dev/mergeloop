//! The build lane's tools, as the daemon calls them for one workspace, signed in.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::Client;

/// A task the daemon claimed: what `claim_build` gives.
#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub build: u64,
    pub claim: u64,
    /// `owner/name`.
    pub repo: String,
    pub clone_url: String,
    pub issue: u64,
    pub title: String,
    pub body: String,
    pub default_branch: String,
    pub branch: String,
    pub verify: Vec<String>,
    pub author: Identity,
    pub co_author: Identity,
    /// Set when the claim took over a build that had opened its PR.
    pub pr: Option<u64>,
}

/// A build this login holds, as `list_builds` lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct Held {
    pub id: u64,
    pub claim: u64,
    pub pr: Option<u64>,
}

/// A git author.
#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

/// What a build stage says in its heartbeat.
#[derive(Debug, Clone, Copy)]
pub enum Stage {
    Building,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Building => "building",
        }
    }
}

/// One workspace's lane, as one daemon.
#[derive(Clone, Debug)]
pub struct Lane {
    pub client: Client,
    pub workspace: u64,
    /// The daemon's name for itself: its claims, and the builds it holds, are its own.
    pub daemon: String,
}

impl Lane {
    /// A task of the workspace, or `None` when there is none.
    pub async fn claim(&self) -> Result<Option<Task>> {
        let answer = self
            .client
            .json(
                "claim_build",
                json!({ "workspace": self.workspace, "daemon": self.daemon }),
            )
            .await?;
        if answer.is_null() {
            return Ok(None);
        }
        serde_json::from_value(answer.clone())
            .map(Some)
            .with_context(|| format!("claim_build answered no task: {answer}"))
    }

    pub async fn heartbeat(&self, task: &Task, stage: Stage) -> Result<()> {
        self.on(task, "heartbeat_build", json!({ "stage": stage.name() }))
            .await
            .map(drop)
    }

    /// A token that pushes the build's branch, as the App's bot.
    pub async fn mint(&self, task: &Task) -> Result<String> {
        let answer = self.on(task, "mint_push_token", json!({})).await?;
        answer["token"]
            .as_str()
            .map(str::to_owned)
            .context("mint_push_token answered no token")
    }

    /// Has the App open the build's PR, and returns its number.
    pub async fn open_pr(&self, task: &Task, title: &str, body: &str) -> Result<u64> {
        let answer = self
            .on(
                task,
                "open_build_pr",
                json!({ "title": title, "body": body }),
            )
            .await?;
        answer["pr"]
            .as_u64()
            .with_context(|| format!("open_build_pr answered no PR: {answer}"))
    }

    /// Says the build's PR merged, which ends the build.
    pub async fn report_merged(&self, task: &Task) -> Result<()> {
        self.on(task, "report_build", json!({ "standing": "merged" }))
            .await
            .map(drop)
    }

    /// Gives the build back, or marks it stuck with why.
    pub async fn release(&self, task: &Task, stuck: Option<&str>) -> Result<()> {
        let args = match stuck {
            Some(why) => json!({ "stuck": why }),
            None => json!({}),
        };
        self.on(task, "release_build", args).await.map(drop)
    }

    /// The builds this daemon holds in the workspace.
    pub async fn held(&self) -> Result<Vec<Held>> {
        let answer = self
            .client
            .json(
                "list_builds",
                json!({ "workspace": self.workspace, "mine": true, "daemon": self.daemon }),
            )
            .await?;
        serde_json::from_value(answer.clone())
            .with_context(|| format!("list_builds answered no builds: {answer}"))
    }

    /// Gives back a build held from before, which this daemon is not building.
    pub async fn give_back(&self, held: &Held) -> Result<()> {
        self.on_build(held.id, held.claim, "release_build", json!({}))
            .await
            .map(drop)
    }

    async fn on(&self, task: &Task, tool: &str, args: Value) -> Result<Value> {
        self.on_build(task.build, task.claim, tool, args).await
    }

    async fn on_build(&self, build: u64, claim: u64, tool: &str, args: Value) -> Result<Value> {
        let mut args = args;
        args["workspace"] = self.workspace.into();
        args["build"] = build.into();
        args["claim"] = claim.into();
        args["daemon"] = self.daemon.as_str().into();
        self.client.json(tool, args).await
    }
}

/// The workspaces the signed-in developer belongs to, as `get_session` lists them.
pub async fn workspaces(client: &Client) -> Result<Vec<u64>> {
    let session = client.json("get_session", json!({})).await?;
    let listed = session["workspaces"]
        .as_array()
        .context("get_session lists no workspaces")?;
    listed
        .iter()
        .map(|w| {
            w["installation_id"]
                .as_u64()
                .context("a workspace without an installation_id")
        })
        .collect()
}
