//! `mergeloop [PR]`: queues the PR, waits until it needs something of the agent, and prints
//! that need with its brief.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{Client, LONGEST_WAIT};
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

/// Queues the PR, and waits until it needs the agent: prints that need, and returns its exit
/// code.
pub async fn land(
    client: &Client,
    repo: &str,
    number: u64,
    mut after: Option<String>,
) -> Result<u8> {
    let mut said = String::new();
    loop {
        let mut args = json!({ "repo": repo, "number": number, "wait": LONGEST_WAIT });
        if let Some(after) = &after {
            args["after"] = after.as_str().into();
        }
        let need = client.json("next_need", args).await?;
        let id = need["id"].as_str().unwrap_or_default().to_owned();
        match need["needs"].as_str() {
            Some("wait") => {
                // For whoever watches the terminal, once per change. The brief's command is
                // for a caller of `next_need`: this keeps waiting itself.
                let brief = need["brief"].as_str().unwrap_or_default();
                if brief != said {
                    eprintln!("mergeloop: {}; waiting", waiting(brief));
                    said = brief.to_owned();
                }
            }
            Some("queue") => {
                if after.as_deref() != Some(id.as_str()) {
                    client
                        .json("queue_pull", json!({ "repo": repo, "number": number }))
                        .await?;
                    eprintln!("mergeloop: queued {repo}#{number}");
                }
                after = Some(id);
            }
            Some(_) => {
                println!("{}", serde_json::to_string_pretty(&printed(&need))?);
                let code = need["exit_code"].as_u64().unwrap_or(2);
                return Ok(u8::try_from(code).unwrap_or(2));
            }
            None => bail!("the server answered no need: {need}"),
        }
    }
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
