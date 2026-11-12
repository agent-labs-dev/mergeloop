//! MergeLoop's MCP endpoint, called as a signed-in user or as a GitHub token's.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::gh::gh;
use crate::session::{self, Secret};

/// The longest a call waits at the server for something to change. It allows up to 50.
pub const LONGEST_WAIT: u64 = 50;
/// Calls in a row that may fail, as on a dropped connection, before the command gives up.
const RETRIES: u32 = 5;

/// A MergeLoop server, by its URL without a trailing slash: how sessions are keyed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server(String);

impl Server {
    pub fn new(url: &str) -> Self {
        Server(url.trim_end_matches('/').to_owned())
    }

    /// `MERGELOOP_URL`, or the hosted MergeLoop.
    pub fn from_env() -> Self {
        let url = std::env::var("MERGELOOP_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "https://mergeloop.ai".into());
        Server::new(&url)
    }

    pub fn url(&self) -> &str {
        &self.0
    }
}

/// What a call is made as.
#[derive(Debug)]
pub enum Credential {
    /// A MergeLoop session, from `mergeloop login`: the server sees a signed-in user, who
    /// reaches the tools for a whole workspace.
    Session(Secret),
    /// A GitHub token: the server sees a token, which reaches single repos.
    GitHub(Secret),
}

/// The HTTP client every call to the server goes through.
pub(crate) fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        // A call may wait at the server for up to `LONGEST_WAIT`.
        .timeout(Duration::from_secs(LONGEST_WAIT + 40))
        .user_agent(concat!("mergeloop-cli/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// Debug shows the credential's kind only: `Secret` hides the token.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    server: Server,
    credential: Arc<Credential>,
}

impl Client {
    pub fn new(server: Server, credential: Credential) -> Result<Self> {
        Ok(Client {
            http: http()?,
            server,
            credential: Arc::new(credential),
        })
    }

    /// What `mergeloop [PR]` and `mergeloop call` send, the first there is: `MERGELOOP_TOKEN`,
    /// the server's session, or `gh auth token`.
    pub fn for_command(server: Server, sessions: &Path) -> Result<Self> {
        let credential = match std::env::var("MERGELOOP_TOKEN") {
            Ok(t) if !t.is_empty() => Credential::GitHub(Secret::new(t)),
            _ => match session::find(sessions, &server)? {
                Some(s) => Credential::Session(s),
                None => Credential::GitHub(Secret::new(gh(&["auth", "token"]).context(
                    "no MERGELOOP_TOKEN, no `mergeloop login`, and `gh auth token` has none: run `gh auth login`",
                )?)),
            },
        };
        Client::new(server, credential)
    }

    /// The server's session, for calls that only a signed-in user may make.
    pub fn signed_in(server: &Server, sessions: &Path) -> Result<Self> {
        let Some(session) = session::find(sessions, server)? else {
            bail!("not signed in to {}: run `mergeloop login`", server.url());
        };
        Client::new(server.clone(), Credential::Session(session))
    }

    pub fn server(&self) -> &Server {
        &self.server
    }

    /// A tool's answer, parsed: its error is the command's.
    pub async fn json(&self, tool: &str, args: Value) -> Result<Value> {
        let (text, is_error) = self.tool(tool, args).await?;
        if is_error {
            bail!("{tool}: {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("{tool} answered no JSON: {text}"))
    }

    /// A tool's text, and whether it is an error, retrying what failed on the way.
    pub async fn tool(&self, tool: &str, args: Value) -> Result<(String, bool)> {
        let mut failed = 0;
        loop {
            match self.call(tool, &args).await {
                Ok(answer) => return Ok(answer),
                Err(Retry::No(e)) => return Err(e),
                Err(Retry::Yes(e)) if failed + 1 >= RETRIES => return Err(e),
                Err(Retry::Yes(e)) => {
                    failed += 1;
                    eprintln!("mergeloop: {e:#}; trying again");
                    tokio::time::sleep(Duration::from_secs(5 * u64::from(failed))).await;
                }
            }
        }
    }

    /// Ends the session at the server. A GitHub token has nothing to end.
    pub async fn end_session(&self) -> Result<()> {
        let Credential::Session(session) = &*self.credential else {
            return Ok(());
        };
        let url = format!("{}/auth/logout", self.server.url());
        let res = self
            .http
            .post(&url)
            .bearer_auth(session.expose())
            .send()
            .await
            .with_context(|| format!("reach {}", self.server.url()))?;
        if !res.status().is_success() {
            bail!("{} answered {}", self.server.url(), res.status());
        }
        Ok(())
    }

    fn token(&self) -> &str {
        match &*self.credential {
            Credential::Session(s) | Credential::GitHub(s) => s.expose(),
        }
    }

    /// What a 401 means: the credential is no good, and only the user can make a new one.
    fn refused(&self) -> anyhow::Error {
        let url = self.server.url();
        match &*self.credential {
            Credential::Session(_) => {
                anyhow!("{url} no longer takes your sign-in: run `mergeloop login`")
            }
            Credential::GitHub(_) => anyhow!(
                "{url} does not take the GitHub token: check MERGELOOP_TOKEN or `gh auth status`"
            ),
        }
    }

    async fn call(&self, tool: &str, args: &Value) -> Result<(String, bool), Retry> {
        let url = self.server.url();
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": args },
        });
        let res = self
            .http
            .post(format!("{url}/mcp"))
            .bearer_auth(self.token())
            .json(&message)
            .send()
            .await
            .map_err(|e| Retry::Yes(anyhow::Error::new(e).context(format!("reach {url}"))))?;
        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Retry::No(self.refused()));
        }
        let body = res.text().await.map_err(|e| Retry::Yes(e.into()))?;
        if status.is_server_error() {
            return Err(Retry::Yes(anyhow!("{url} answered {status}")));
        }
        if !status.is_success() {
            return Err(Retry::No(anyhow!("{url} answered {status}: {body}")));
        }
        let answer: Value = serde_json::from_str(&body)
            .map_err(|_| Retry::No(anyhow!("{url} answered no JSON: {body}")))?;
        if let Some(error) = answer.get("error") {
            return Err(Retry::No(anyhow!("{}", error["message"])));
        }
        let result = &answer["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        Ok((text.to_owned(), result["isError"] == true))
    }
}

/// Whether a failed call is worth making again.
enum Retry {
    Yes(anyhow::Error),
    No(anyhow::Error),
}
