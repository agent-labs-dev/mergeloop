//! MergeLoop's MCP endpoint, called as a signed-in user or as a GitHub token's.

use std::path::{Path, PathBuf};
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

/// A MergeLoop server, by its canonical URL: lowercase host, no default port, no trailing slash.
/// Every way of writing one server names one session, and every request goes to that URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server(String);

impl Server {
    /// The server at `url`: `http` or `https`, with a host, and nothing a server URL has no use
    /// for (a user, a query, a fragment).
    pub fn parse(url: &str) -> Result<Self> {
        let parsed =
            reqwest::Url::parse(url).with_context(|| format!("{url} is not a server's URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("{url} is not a server's URL: use http:// or https:// and a host");
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            bail!("{url} is not a server's URL: it has a user, a query or a fragment");
        }
        Ok(Server(parsed.as_str().trim_end_matches('/').to_owned()))
    }

    /// `MERGELOOP_URL`, or the hosted MergeLoop.
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("MERGELOOP_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "https://mergeloop.ai".into());
        Server::parse(&url).context("MERGELOOP_URL")
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
    /// the server's session, or `gh auth token`. The sessions are looked for, and `HOME`
    /// needed, only without `MERGELOOP_TOKEN`.
    pub fn for_command(server: Server, sessions: impl FnOnce() -> Result<PathBuf>) -> Result<Self> {
        let credential = match std::env::var("MERGELOOP_TOKEN") {
            Ok(t) if !t.is_empty() => Credential::GitHub(Secret::new(t)),
            _ => match session::find(&sessions()?, &server)? {
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

    /// A tool's answer, parsed. Its error is a `Refused`, which says what kind it is.
    pub async fn json(&self, tool: &str, args: Value) -> Result<Value> {
        let answer = self.answer(tool, args).await?;
        if let Some(refused) = answer.refused {
            return Err(refused.into());
        }
        let text = answer.text;
        serde_json::from_str(&text).with_context(|| format!("{tool} answered no JSON: {text}"))
    }

    /// A tool's text, and whether it is an error, retrying what failed on the way.
    pub async fn tool(&self, tool: &str, args: Value) -> Result<(String, bool)> {
        let answer = self.answer(tool, args).await?;
        Ok((answer.text, answer.refused.is_some()))
    }

    async fn answer(&self, tool: &str, args: Value) -> Result<Answer> {
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
    fn refused(&self, tool: &str) -> anyhow::Error {
        let url = self.server.url();
        match &*self.credential {
            Credential::Session(_) => Refused {
                tool: tool.to_owned(),
                status: Some(401),
                kind: None,
                text: format!("{url} no longer takes your sign-in: run `mergeloop login`"),
            }
            .into(),
            Credential::GitHub(_) => anyhow!(
                "{url} does not take the GitHub token: check MERGELOOP_TOKEN or `gh auth status`"
            ),
        }
    }

    async fn call(&self, tool: &str, args: &Value) -> Result<Answer, Retry> {
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
            return Err(Retry::No(self.refused(tool)));
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
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let refused = (result["isError"] == true).then(|| Refused {
            tool: tool.to_owned(),
            status: result["_meta"]["status"].as_u64(),
            kind: result["_meta"]["refusal"].as_str().and_then(Refusal::parse),
            text: text.clone(),
        });
        Ok(Answer { text, refused })
    }
}

/// A tool's answer: its text, and why it refused, when it did.
struct Answer {
    text: String,
    refused: Option<Refused>,
}

/// A tool refused: what the server said, and what kind of refusal it is, as `_meta` names it.
/// Its `Display` is `<tool>: <text>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub tool: String,
    /// The route's HTTP status, when the server says it.
    pub status: Option<u64>,
    pub kind: Option<Refusal>,
    pub text: String,
}

impl Refused {
    /// The sign-in is no good: only the developer can make a new one.
    pub fn needs_sign_in(&self) -> bool {
        self.status == Some(401)
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.tool, self.text)
    }
}

impl std::error::Error for Refused {}

/// The refusals of the build lane that ask something different of the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The workspace's lane is off.
    LaneOff,
    /// The caller no longer holds the build: stop, and call nothing more on it.
    Lost,
    /// The build's issue is no task any more: give it back.
    Cancelled,
    /// The build went unheard too long: heartbeat it, then ask again.
    Unheard,
    /// GitHub says the caller may not push: give it back.
    MayNotPush,
    /// The build's PR merged: report it merged.
    Merged,
}

impl Refusal {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "lane_off" => Refusal::LaneOff,
            "lost" => Refusal::Lost,
            "cancelled" => Refusal::Cancelled,
            "unheard" => Refusal::Unheard,
            "may_not_push" => Refusal::MayNotPush,
            "merged" => Refusal::Merged,
            _ => return None,
        })
    }
}

/// Whether `e` says the sign-in is no good.
pub fn needs_sign_in(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>()
        .is_some_and(Refused::needs_sign_in)
}

/// The kind of refusal `e` is, if it is one.
pub fn refusal(e: &anyhow::Error) -> Option<Refusal> {
    e.downcast_ref::<Refused>().and_then(|r| r.kind)
}

/// Whether a failed call is worth making again.
enum Retry {
    Yes(anyhow::Error),
    No(anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> String {
        Server::parse(s).unwrap().url().to_owned()
    }

    #[test]
    fn one_server_written_many_ways_is_one_server() {
        for alias in [
            "https://mergeloop.ai",
            "https://mergeloop.ai/",
            "https://MERGELOOP.ai",
            "https://mergeloop.ai:443",
            "HTTPS://MergeLoop.AI:443/",
        ] {
            assert_eq!(url(alias), "https://mergeloop.ai", "{alias}");
        }
        assert_eq!(url("http://127.0.0.1:8080/"), "http://127.0.0.1:8080");
        assert_eq!(url("http://localhost:80"), "http://localhost");
    }

    #[test]
    fn what_reaches_another_server_stays_another_server() {
        let base = url("https://mergeloop.ai");
        for other in [
            "http://mergeloop.ai",
            "https://mergeloop.ai:8443",
            "https://eu.mergeloop.ai",
            "https://mergeloop.ai/ml",
        ] {
            assert_ne!(url(other), base, "{other}");
        }
        assert_eq!(url("https://x.dev/ml/"), "https://x.dev/ml");
    }

    #[test]
    fn a_url_with_a_user_query_or_fragment_or_no_host_is_refused() {
        for bad in [
            "https://me:pw@mergeloop.ai",
            "https://mergeloop.ai/?a=1",
            "https://mergeloop.ai/#x",
            "ftp://mergeloop.ai",
            "mergeloop.ai",
            "",
        ] {
            assert!(Server::parse(bad).is_err(), "{bad}");
        }
    }
}
