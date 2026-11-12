//! Issues and PRs, for the build lane: the labeled issues are its tasks, a claim assigns its
//! developer, and the App opens and checks each build's PR.

use anyhow::Result;
use mergepatrol_core::RepoName;
use reqwest::Method;
use serde_json::{Value, json};

use crate::{Installation, percent_encode};

/// An issue as GitHub has it, from the REST API or an `issues` webhook, which share its shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub labels: Vec<String>,
    /// Logins.
    pub assignees: Vec<String>,
}

impl Issue {
    /// `None` for a PR, which the issues API lists too, or for what is no issue.
    pub fn parse(issue: &Value) -> Option<Self> {
        if !issue["pull_request"].is_null() {
            return None;
        }
        let names = |list: &Value, key: &str| -> Vec<String> {
            list.as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v[key].as_str().map(str::to_owned))
                .collect()
        };
        Some(Self {
            number: issue["number"].as_u64()?,
            title: issue["title"].as_str().unwrap_or_default().to_owned(),
            body: issue["body"].as_str().unwrap_or_default().to_owned(),
            open: issue["state"] == "open",
            labels: names(&issue["labels"], "name"),
            assignees: names(&issue["assignees"], "login"),
        })
    }

    /// Open, with `label`. GitHub's label names ignore case.
    pub fn labeled(&self, label: &str) -> bool {
        self.open && self.labels.iter().any(|l| l.eq_ignore_ascii_case(label))
    }
}

/// What a list read: all of it, or as many pages as it might read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed<T> {
    pub items: Vec<T>,
    /// It read the last page.
    pub complete: bool,
}

/// A PR, as the build lane checks it before it takes it for a build's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanePull {
    pub number: u64,
    pub open: bool,
    pub merged: bool,
    /// Who opened it: the App's is `<slug>[bot]`.
    pub author: String,
    /// The repo its head branch is in, `None` once that is deleted.
    pub head_repo: Option<String>,
    pub head: String,
    pub base: String,
}

impl LanePull {
    fn parse(pr: &Value) -> Option<Self> {
        Some(Self {
            number: pr["number"].as_u64()?,
            open: pr["state"] == "open",
            merged: !pr["merged_at"].is_null(),
            author: pr["user"]["login"].as_str()?.to_owned(),
            head_repo: pr["head"]["repo"]["full_name"].as_str().map(str::to_owned),
            head: pr["head"]["ref"].as_str()?.to_owned(),
            base: pr["base"]["ref"].as_str()?.to_owned(),
        })
    }
}

impl Installation {
    /// The repo's open issues with `label`, PRs left out, from at most `max_pages` pages of 100.
    pub async fn labeled_issues(
        &self,
        repo: &RepoName,
        label: &str,
        max_pages: usize,
    ) -> Result<Listed<Issue>> {
        let path = format!(
            "/repos/{repo}/issues?labels={}&state=open",
            percent_encode(label)
        );
        let Listed { items, complete } = self.pages(&path, max_pages).await?;
        Ok(Listed {
            items: items.iter().filter_map(Issue::parse).collect(),
            complete,
        })
    }

    /// When `label` was put on the issue, each time, as RFC 3339, from at most `max_pages`
    /// pages of its events.
    pub async fn labeled_at(
        &self,
        repo: &RepoName,
        issue: u64,
        label: &str,
        max_pages: usize,
    ) -> Result<Vec<String>> {
        let path = format!("/repos/{repo}/issues/{issue}/events?");
        let events = self.pages(&path, max_pages).await?.items;
        Ok(events
            .iter()
            .filter(|e| e["event"] == "labeled")
            .filter(|e| {
                e["label"]["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(label))
            })
            .filter_map(|e| e["created_at"].as_str().map(str::to_owned))
            .collect())
    }

    /// A list API's items, 100 a page, from at most `max_pages` pages.
    async fn pages(&self, path: &str, max_pages: usize) -> Result<Listed<Value>> {
        let mut items = Vec::new();
        for page in 1..=max_pages {
            let sep = if path.ends_with('?') { "" } else { "&" };
            let batch: Vec<Value> = self
                .rest(
                    Method::GET,
                    &format!("{path}{sep}per_page=100&page={page}"),
                    None,
                )
                .await?;
            let last = batch.len() < 100;
            items.extend(batch);
            if last {
                return Ok(Listed {
                    items,
                    complete: true,
                });
            }
        }
        Ok(Listed {
            items,
            complete: false,
        })
    }

    /// The repo's PRs from `branch`, of any state.
    pub async fn pulls_from(&self, repo: &RepoName, branch: &str) -> Result<Vec<LanePull>> {
        let prs: Vec<Value> = self
            .rest(
                Method::GET,
                &format!("/repos/{repo}/pulls?state=all&head={}:{branch}", repo.owner),
                None,
            )
            .await?;
        Ok(prs.iter().filter_map(LanePull::parse).collect())
    }

    pub async fn lane_pull(&self, repo: &RepoName, number: u64) -> Result<LanePull> {
        let pr: Value = self
            .rest(Method::GET, &format!("/repos/{repo}/pulls/{number}"), None)
            .await?;
        LanePull::parse(&pr).ok_or_else(|| anyhow::anyhow!("GitHub gave no PR #{number}"))
    }

    /// Assigns `login` on the issue, beside its other assignees.
    pub async fn assign(&self, repo: &RepoName, issue: u64, login: &str) -> Result<()> {
        self.assignees(Method::POST, repo, issue, login).await
    }

    pub async fn unassign(&self, repo: &RepoName, issue: u64, login: &str) -> Result<()> {
        self.assignees(Method::DELETE, repo, issue, login).await
    }

    async fn assignees(
        &self,
        method: Method,
        repo: &RepoName,
        issue: u64,
        login: &str,
    ) -> Result<()> {
        let path = format!("/repos/{repo}/issues/{issue}/assignees");
        let _: Value = self
            .rest(method, &path, Some(json!({ "assignees": [login] })))
            .await?;
        Ok(())
    }

    /// The account ID of `login`, for its `noreply` email, which GitHub links commits to.
    pub async fn user_id(&self, login: &str) -> Result<u64> {
        let user: Value = self
            .rest(
                Method::GET,
                &format!("/users/{}", percent_encode(login)),
                None,
            )
            .await?;
        user["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("GitHub gave no ID for {login}"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::fake;

    #[test]
    fn an_issue_is_labeled_while_open_and_a_pr_is_no_issue() {
        let issue = json!({
            "number": 4, "title": "Add X", "body": null, "state": "open",
            "labels": [{ "name": "MergeLoop" }], "assignees": [{ "login": "ana" }],
        });
        let parsed = Issue::parse(&issue).unwrap();
        assert_eq!(parsed.body, "");
        assert_eq!(parsed.assignees, ["ana"]);
        assert!(parsed.labeled("mergeloop"));
        assert!(!parsed.labeled("other"));
        let closed = Issue {
            open: false,
            ..parsed
        };
        assert!(!closed.labeled("mergeloop"));

        let pr = json!({ "number": 5, "pull_request": { "url": "u" } });
        assert_eq!(Issue::parse(&pr), None);
    }

    #[tokio::test]
    async fn labeled_issues_are_read_page_by_page_without_prs() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seen = paths.clone();
        let gh = fake::answered_by(move |_, path, _| {
            seen.lock().unwrap().push(path.to_owned());
            let page: Vec<Value> = if path.ends_with("page=1") {
                (1..=100)
                    .map(|n| json!({ "number": n, "state": "open", "pull_request": if n == 1 { json!({}) } else { Value::Null } }))
                    .collect()
            } else {
                vec![json!({ "number": 101, "state": "open" })]
            };
            (200, Value::from(page).to_string())
        })
        .await;
        let repo = RepoName::parse("o/r").unwrap();
        let issues = gh.labeled_issues(&repo, "build me", 10).await.unwrap();
        assert_eq!(issues.items.len(), 100, "#1 is a PR");
        assert!(issues.complete);
        assert_eq!(
            paths.lock().unwrap()[0],
            "/repos/o/r/issues?labels=build%20me&state=open&per_page=100&page=1"
        );
    }

    #[tokio::test]
    async fn a_pr_is_read_with_its_head_base_author_and_whether_it_merged() {
        let gh = fake::answered_by(|_, path, _| {
            let pr = json!({
                "number": 40, "state": "closed", "merged_at": "2026-11-12T12:00:00Z",
                "user": { "login": "mp[bot]" },
                "head": { "ref": "mergeloop/build-7", "repo": null },
                "base": { "ref": "trunk" },
            });
            match path {
                "/repos/o/r/pulls/40" => (200, pr.to_string()),
                _ => (200, json!([pr, { "number": 41 }]).to_string()),
            }
        })
        .await;
        let repo = RepoName::parse("o/r").unwrap();
        let want = LanePull {
            number: 40,
            open: false,
            merged: true,
            author: "mp[bot]".into(),
            head_repo: None,
            head: "mergeloop/build-7".into(),
            base: "trunk".into(),
        };
        assert_eq!(gh.lane_pull(&repo, 40).await.unwrap(), want);
        let from = gh.pulls_from(&repo, "mergeloop/build-7").await.unwrap();
        assert_eq!(from, [want], "what is no PR is left out");
    }

    #[tokio::test]
    async fn a_list_stops_at_its_page_budget_and_says_it_saw_only_part() {
        let gh = fake::answered_by(|_, _, _| {
            let page: Vec<Value> = (0..100)
                .map(|n| json!({ "number": n, "state": "open" }))
                .collect();
            (200, Value::from(page).to_string())
        })
        .await;
        let repo = RepoName::parse("o/r").unwrap();
        let issues = gh.labeled_issues(&repo, "l", 3).await.unwrap();
        assert_eq!((issues.items.len(), issues.complete), (300, false));
    }

    #[tokio::test]
    async fn labeled_at_gives_when_the_label_was_put_on_in_any_case() {
        let gh = fake::answered_by(|_, path, _| {
            assert_eq!(path, "/repos/o/r/issues/7/events?per_page=100&page=1");
            let events = json!([
                { "event": "labeled", "created_at": "t1", "label": { "name": "MergeLoop" } },
                { "event": "unlabeled", "created_at": "t2", "label": { "name": "mergeloop" } },
                { "event": "labeled", "created_at": "t3", "label": { "name": "bug" } },
            ]);
            (200, events.to_string())
        })
        .await;
        let repo = RepoName::parse("o/r").unwrap();
        let at = gh.labeled_at(&repo, 7, "mergeloop", 10).await.unwrap();
        assert_eq!(at, ["t1"]);
    }
}
