//! Issues, for the build lane: the labeled ones are its tasks, and a claim assigns its developer.

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

impl Installation {
    /// The repo's open issues with `label`, PRs left out.
    pub async fn labeled_issues(&self, repo: &RepoName, label: &str) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();
        for page in 1.. {
            let batch: Vec<Value> = self
                .rest(
                    Method::GET,
                    &format!(
                        "/repos/{repo}/issues?labels={}&state=open&per_page=100&page={page}",
                        percent_encode(label)
                    ),
                    None,
                )
                .await?;
            issues.extend(batch.iter().filter_map(Issue::parse));
            if batch.len() < 100 {
                break;
            }
        }
        Ok(issues)
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
        let issues = gh.labeled_issues(&repo, "build me").await.unwrap();
        assert_eq!(issues.len(), 100, "#1 is a PR");
        assert_eq!(
            paths.lock().unwrap()[0],
            "/repos/o/r/issues?labels=build%20me&state=open&per_page=100&page=1"
        );
    }
}
