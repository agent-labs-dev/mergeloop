//! The build lane's local review: a second agent reads a build's change against its issue,
//! before the daemon pushes it, and answers with what the builder must fix, or nothing. The
//! reviewer cannot write the build's tree; the daemon checks that it did not.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::Outcome;

/// A build's change to review: the commit at the tree's `HEAD`, on top of `base`.
pub struct BuildReview<'a> {
    /// `owner/name`.
    pub repo: &'a str,
    pub issue: u64,
    pub title: &'a str,
    pub body: &'a str,
    /// The build's worktree, which the reviewer reads but may not write.
    pub tree: &'a Path,
    pub base: &'a str,
    /// The repo's `verify` commands, which passed on the change.
    pub verify: &'a [String],
}

/// What the reviewer asks the builder to fix.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct BuildFinding {
    /// The file, in the repo, and the line on the change's side.
    pub file: String,
    pub line: u32,
    /// What is wrong, and what to change.
    pub finding: String,
}

#[derive(Deserialize)]
struct Reply {
    findings: Vec<BuildFinding>,
}

pub fn review_build_prompt(req: &BuildReview<'_>) -> String {
    let BuildReview {
        repo,
        issue,
        title,
        body,
        base,
        ..
    } = req;
    let tree = req.tree.display();
    let verify = if req.verify.is_empty() {
        String::new()
    } else {
        let listed: String = req.verify.iter().map(|c| format!("- `{c}`\n")).collect();
        format!("These commands pass on it:\n{listed}\n")
    };
    format!(
        "You are reviewing a change to {repo} that another agent made for this issue, before \
MergeLoop opens a pull request with it.

Issue #{issue}: {title}

{body}

The change is the last commit of the git worktree `{tree}`: see it with `git -C {tree} diff \
{base} HEAD`, and read the files around it there. You may read that worktree but not write it, \
so do not change, build or test anything in it. {verify}Where the repo has `REVIEW.md`, \
`AGENTS.md` or `CLAUDE.md`, read them as they are on the base (`git -C {tree} show \
{base}:REVIEW.md`): they say what matters in this repo.

Report only what the change's author must fix before it merges: a bug, a part of the issue it \
does not do, risky code with no test, a break of a convention those files state. Leave out \
style, and what you found sound.

Reply with the JSON alone, in this shape, `file` from the repo's root and `line` on the change's \
side:
{{\"findings\": [{{\"file\": \"src/lib.rs\", \"line\": 12, \"finding\": \"What is wrong, and what to change.\"}}]}}
When the change needs nothing, reply `{{\"findings\": []}}`."
    )
}

/// The reviewer's findings, from its last message. An error when it did not review.
pub fn review_build_findings(outcome: &Outcome, text: &str) -> Result<Vec<BuildFinding>> {
    let reply: Reply = reply_json(outcome, text)?;
    Ok(reply.findings)
}

/// The review JSON an agent replied with, alone or in a code block, once it ran to the end.
pub fn reply_json<T: DeserializeOwned>(outcome: &Outcome, text: &str) -> Result<T> {
    match outcome {
        Outcome::Failed(why) => bail!("the agent failed: {why}"),
        Outcome::GaveUp(why) => bail!("the agent gave up: {why}"),
        Outcome::Resolved => {}
    }
    let json = match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => &text[start..=end],
        _ => bail!("the agent's review is not JSON"),
    };
    serde_json::from_str(json).context("the agent's review does not parse")
}

/// What the builder's next run is told of the reviewer's findings.
pub fn findings_note(findings: &[BuildFinding]) -> String {
    let listed: String = findings
        .iter()
        .map(|f| format!("- `{}:{}`: {}\n", f.file, f.line, f.finding.trim()))
        .collect();
    format!(
        "Your earlier run's change is this checkout's last commit. A reviewer read it and asks \
you to fix these:\n{listed}Fix each one in the checkout, keeping the rest of the change."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_the_issue_the_tree_and_the_reply_it_reads() {
        let verify = vec!["cargo test".to_owned()];
        let p = review_build_prompt(&BuildReview {
            repo: "o/r",
            issue: 7,
            title: "Add a flag",
            body: "Add `--dry-run`.",
            tree: Path::new("/ws/builds/1/tree"),
            base: "abc123",
            verify: &verify,
        });
        for said in [
            "o/r",
            "Issue #7: Add a flag",
            "Add `--dry-run`.",
            "`git -C /ws/builds/1/tree diff abc123 HEAD`",
            "- `cargo test`",
            "`git -C /ws/builds/1/tree show abc123:REVIEW.md`",
            "{\"findings\": []}",
        ] {
            assert!(p.contains(said), "{said}:\n{p}");
        }
        // The shape the prompt shows is the one that parses.
        let start = p.find("{\"findings\": [{").unwrap();
        let shown = p[start..].lines().next().unwrap();
        let found = review_build_findings(&Outcome::Resolved, shown).unwrap();
        assert_eq!((found[0].file.as_str(), found[0].line), ("src/lib.rs", 12));
        assert_eq!(
            review_build_findings(&Outcome::Resolved, "{\"findings\": []}").unwrap(),
            []
        );
    }

    #[test]
    fn a_reply_reads_from_json_in_a_code_block_and_a_reviewer_that_did_not_review_is_an_error() {
        let text = "Found one:\n```json\n{\"findings\": [{\"file\": \"a.rs\", \"line\": 3, \"finding\": \"f\"}]}\n```";
        let found = review_build_findings(&Outcome::Resolved, text).unwrap();
        assert_eq!(found.len(), 1);
        assert!(review_build_findings(&Outcome::Resolved, "Looks good to me.").is_err());
        assert!(review_build_findings(&Outcome::Resolved, "{\"summary\": \"ok\"}").is_err());
        let why = review_build_findings(&Outcome::GaveUp("no access".into()), text)
            .unwrap_err()
            .to_string();
        assert_eq!(why, "the agent gave up: no access");
    }

    #[test]
    fn the_builder_is_told_each_finding_where_it_is() {
        let note = findings_note(&[BuildFinding {
            file: "a.rs".into(),
            line: 3,
            finding: "Off by one.\n".into(),
        }]);
        assert!(note.contains("- `a.rs:3`: Off by one.\n"), "{note}");
        assert!(note.contains("last commit"), "{note}");
    }
}
