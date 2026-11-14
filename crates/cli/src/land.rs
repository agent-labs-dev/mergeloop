//! `mergeloop [PR]`: queues the PR, waits until it needs something of the agent, and prints
//! that need with its brief.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{Client, LONGEST_WAIT, retry_after};
use crate::gh::gh;

/// A PR as the command line names it.
#[derive(Debug, PartialEq)]
pub struct PrRef {
    /// `owner/name`, or the current directory's repo.
    pub repo: Option<String>,
    pub number: u64,
}

impl PrRef {
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || anyhow::anyhow!("{s} is not a PR: use 123, owner/name#123, or its URL");
        if let Some(path) = s
            .strip_prefix("https://github.com/")
            .or_else(|| s.strip_prefix("github.com/"))
        {
            let parts: Vec<&str> = path.split('/').collect();
            let [owner, name, "pull", number, ..] = parts[..] else {
                return Err(bad());
            };
            return Ok(PrRef {
                repo: Some(format!("{owner}/{name}")),
                number: number.parse().map_err(|_| bad())?,
            });
        }
        let (repo, number) = match s.split_once('#') {
            Some(("", n)) => (None, n),
            Some((repo, n)) if repo.split('/').filter(|p| !p.is_empty()).count() == 2 => {
                (Some(repo.to_owned()), n)
            }
            Some(_) => return Err(bad()),
            None => (None, s),
        };
        Ok(PrRef {
            repo,
            number: number.parse().map_err(|_| bad())?,
        })
    }
}

/// The repo and number, asking `gh` for what the command line left out.
pub fn resolve(pr: Option<PrRef>) -> Result<(String, u64)> {
    let pr = match pr {
        Some(pr) => pr,
        None => {
            let url = gh(&["pr", "view", "--json", "url", "-q", ".url"])
                .context("no PR given, and `gh` finds none for the current branch")?;
            PrRef::parse(&url)?
        }
    };
    let repo = match pr.repo {
        Some(repo) => repo,
        None => gh(&[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ])
        .context("no repo given, and `gh` finds none for the current directory")?,
    };
    Ok((repo, pr.number))
}

/// A wait's brief without the command to run next, which `land` runs itself.
fn waiting(brief: &str) -> &str {
    brief.split_once(". Run `").map_or(brief, |(said, _)| said)
}

/// A PR in the land loop, and how its tools are called.
pub struct Pr<'a> {
    /// `owner/name`.
    pub repo: &'a str,
    pub number: u64,
    /// Its workspace, for a signed-in caller that knows it. Without it, the server finds it.
    pub workspace: Option<u64>,
    /// Briefs for an agent that `mergeloop daemon` runs, which commits and pushes for it.
    pub for_daemon: bool,
}

impl Pr<'_> {
    fn args(&self) -> Value {
        let mut args = json!({ "repo": self.repo, "number": self.number });
        if let Some(workspace) = self.workspace {
            args["workspace"] = workspace.into();
        }
        args
    }
}

/// The PR's next need that the loop does not handle itself: a wait is waited out, and a PR that
/// is not queued is queued, once per need. `after` is the need seen last, and moves on with the
/// needs.
/// `first` is a need already asked for, which the loop starts from.
pub async fn next(
    client: &Client,
    pr: &Pr<'_>,
    after: &mut Option<String>,
    mut first: Option<Value>,
) -> Result<Value> {
    let mut said = String::new();
    loop {
        let need = match first.take() {
            Some(need) => need,
            None => ask(client, pr, after.as_deref()).await?,
        };
        let id = need["id"].as_str().unwrap_or_default().to_owned();
        match need["needs"].as_str() {
            Some("wait") => {
                // For whoever watches the terminal, once per change. The brief's command is
                // for a caller of `next_need`: this keeps waiting itself.
                let brief = need["brief"].as_str().unwrap_or_default();
                if brief != said {
                    eprintln!(
                        "mergeloop: {}#{}: {}; waiting",
                        pr.repo,
                        pr.number,
                        waiting(brief)
                    );
                    said = brief.to_owned();
                }
            }
            Some("queue") => {
                if after.as_deref() != Some(id.as_str()) {
                    once_allowed(client, "queue_pull", pr.args()).await?;
                    eprintln!("mergeloop: queued {}#{}", pr.repo, pr.number);
                }
                *after = Some(id);
            }
            Some(_) => return Ok(need),
            None => bail!("the server answered no need: {need}"),
        }
    }
}

/// The PR's need once it is not `after`, or `after` again, as a `wait`, when that is still its
/// need after the longest wait: one call, as the server answers it.
pub async fn ask(client: &Client, pr: &Pr<'_>, after: Option<&str>) -> Result<Value> {
    let mut args = pr.args();
    args["wait"] = LONGEST_WAIT.into();
    if pr.for_daemon {
        args["for_daemon"] = true.into();
    }
    if let Some(after) = after {
        args["after"] = after.into();
    }
    once_allowed(client, "next_need", args).await
}

/// The tool's answer, asked again once GitHub's rate limit lifts while it is used up. A used up
/// rate limit is no failure of the PR's, and an agent told of one as a failure may route around
/// the queue.
async fn once_allowed(client: &Client, tool: &str, args: Value) -> Result<Value> {
    loop {
        let e = match client.json(tool, args.clone()).await {
            Ok(answer) => return Ok(answer),
            Err(e) => e,
        };
        let Some(wait) = retry_after(&e) else {
            return Err(e);
        };
        eprintln!(
            "mergeloop: {}; waiting {}s",
            e.to_string().trim_end_matches('.'),
            wait.as_secs()
        );
        // A second past the reset, so the limit has lifted by the time the call lands.
        tokio::time::sleep(wait + Duration::from_secs(1)).await;
    }
}

/// Queues the PR, and waits until it needs the agent: prints that need, and returns its exit
/// code.
pub async fn land(
    client: &Client,
    repo: &str,
    number: u64,
    mut after: Option<String>,
) -> Result<u8> {
    let pr = Pr {
        repo,
        number,
        workspace: None,
        for_daemon: false,
    };
    let need = next(client, &pr, &mut after, None).await?;
    println!("{}", serde_json::to_string_pretty(&printed(&need))?);
    let code = need["exit_code"].as_u64().unwrap_or(2);
    Ok(u8::try_from(code).unwrap_or(2))
}

/// The need as the agent reads it: what to do, without the whole status.
fn printed(need: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for field in ["needs", "repo", "number", "layer", "brief", "then", "id"] {
        if !need[field].is_null() {
            out.insert(field.into(), need[field].clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_says_what_it_waits_on_not_what_to_run() {
        let brief = "Nothing for you yet: its checks are running. Run `mergeloop o/r#5 --after x`.";
        assert_eq!(
            waiting(brief),
            "Nothing for you yet: its checks are running"
        );
        assert_eq!(waiting("Queued."), "Queued.");
    }

    #[test]
    fn a_pr_is_a_number_a_repos_number_or_a_url() {
        let pr = |repo: Option<&str>, number| PrRef {
            repo: repo.map(str::to_owned),
            number,
        };
        assert_eq!(PrRef::parse("123").unwrap(), pr(None, 123));
        assert_eq!(PrRef::parse("#123").unwrap(), pr(None, 123));
        assert_eq!(PrRef::parse("o/r#5").unwrap(), pr(Some("o/r"), 5));
        assert_eq!(
            PrRef::parse("https://github.com/o/r/pull/5/files").unwrap(),
            pr(Some("o/r"), 5)
        );
        for bad in [
            "o#5",
            "o/r/x#5",
            "r#x",
            "https://github.com/o/r/issues/5",
            "",
        ] {
            assert!(PrRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_printed_need_leaves_out_the_status() {
        let need = json!({
            "needs": "threads", "id": "x", "repo": "o/r", "number": 5, "layer": null,
            "brief": "b", "then": "t", "exit_code": 0, "status": {},
        });
        assert_eq!(
            printed(&need),
            json!({ "needs": "threads", "id": "x", "repo": "o/r", "number": 5, "brief": "b", "then": "t" })
        );
    }
}
