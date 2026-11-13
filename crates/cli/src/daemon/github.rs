//! What the daemon says on a built PR's review threads, as the App's bot: with the token minted
//! for the build's repo (`pull_requests: write` covers replying in a thread and resolving it),
//! never with the developer's own login.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// GitHub's API, as the App's bot on one repo.
pub struct Bot<'a> {
    pub http: &'a reqwest::Client,
    /// `https://api.github.com`, or a test's.
    pub api: &'a str,
    pub token: &'a str,
}

/// A review thread of a PR, as the bot sees it.
pub struct Thread {
    pub id: String,
    pub resolved: bool,
    /// The body of the bot's newest comment among the thread's last 20.
    pub bot_said: Option<String>,
}

impl Bot<'_> {
    /// The PR's review threads: every page of them.
    pub async fn threads(&self, repo: &str, pr: u64) -> Result<Vec<Thread>> {
        let (owner, name) = repo.split_once('/').context("a repo is owner/name")?;
        let mut threads = Vec::new();
        let mut after = Value::Null;
        loop {
            let data = self
                .graphql(
                    "query($owner: String!, $name: String!, $pr: Int!, $after: String) { repository(owner: $owner, name: $name) { pullRequest(number: $pr) { reviewThreads(first: 100, after: $after) { nodes { id isResolved comments(last: 20) { nodes { body viewerDidAuthor } } } pageInfo { hasNextPage endCursor } } } } }",
                    json!({ "owner": owner, "name": name, "pr": pr, "after": after }),
                )
                .await?;
            let page = &data["repository"]["pullRequest"]["reviewThreads"];
            let nodes = page["nodes"].as_array().into_iter().flatten();
            threads.extend(nodes.filter_map(|n| {
                let comments = n["comments"]["nodes"].as_array().into_iter().flatten();
                let bot_said = comments
                    .filter(|c| c["viewerDidAuthor"] == true)
                    .filter_map(|c| c["body"].as_str())
                    .next_back();
                Some(Thread {
                    id: n["id"].as_str()?.to_owned(),
                    resolved: n["isResolved"] == true,
                    bot_said: bot_said.map(str::to_owned),
                })
            }));
            if page["pageInfo"]["hasNextPage"] != true {
                return Ok(threads);
            }
            after = page["pageInfo"]["endCursor"].clone();
            if !after.is_string() {
                bail!("GitHub said PR #{pr} has more review threads, but not where they start");
            }
        }
    }

    pub async fn reply(&self, thread: &str, body: &str) -> Result<()> {
        self.graphql(
            "mutation($thread: ID!, $body: String!) { addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $thread, body: $body }) { comment { id } } }",
            json!({ "thread": thread, "body": body }),
        )
        .await
        .map(drop)
    }

    pub async fn resolve(&self, thread: &str) -> Result<()> {
        self.graphql(
            "mutation($thread: ID!) { resolveReviewThread(input: { threadId: $thread }) { thread { id } } }",
            json!({ "thread": thread }),
        )
        .await
        .map(drop)
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let res = self
            .http
            .post(format!("{}/graphql", self.api))
            .bearer_auth(self.token)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or_default();
        if !status.is_success() || body.get("errors").is_some_and(|e| !e.is_null()) {
            bail!(
                "GitHub's GraphQL answered {status}: {}",
                body.get("errors").unwrap_or(&body["message"])
            );
        }
        Ok(body["data"].clone())
    }
}
