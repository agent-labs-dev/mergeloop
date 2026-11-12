//! The prompt that builds an issue, for the build lane's daemon. The agent changes files only:
//! the daemon runs the verify commands, commits as the App's bot with the developer as
//! co-author, and pushes.

use crate::{COMMIT, GIVE_UP};

/// An issue to build, in a checkout of the repo's default branch.
pub struct BuildRequest<'a> {
    /// `owner/name`.
    pub repo: &'a str,
    pub issue: u64,
    pub title: &'a str,
    pub body: &'a str,
    /// The repo's `verify` commands.
    pub verify: &'a [String],
    /// What failed when the daemon ran them after the agent's last run, if anything did.
    pub failed: Option<&'a VerifyFailed>,
}

/// A verify command that failed, and the end of what it printed.
pub struct VerifyFailed {
    pub command: String,
    pub output: String,
}

pub fn build_prompt(req: &BuildRequest<'_>) -> String {
    let BuildRequest {
        repo,
        issue,
        title,
        body,
        ..
    } = req;
    let verify = if req.verify.is_empty() {
        "This repo sets no verify commands. Run the tests that cover what you changed, and say \
what you could not check."
            .to_owned()
    } else {
        let listed: String = req.verify.iter().map(|c| format!("- `{c}`\n")).collect();
        format!(
            "These commands must pass. MergeLoop runs them after you, in this checkout, and pushes \
nothing until they do:\n{listed}"
        )
    };
    let failed = match req.failed {
        None => String::new(),
        Some(f) => format!(
            "\nYour earlier run left the change in this checkout, and `{}` failed on it. Fix the \
change so it passes. The end of its output:\n```\n{}\n```\n",
            f.command,
            f.output.trim_end()
        ),
    };
    format!(
        "You are building a GitHub issue of {repo} in this checkout, a git worktree of its \
default branch. MergeLoop opens a pull request with your change once it passes its checks.

Issue #{issue}: {title}

{body}
{failed}
Make the change the issue asks for, as the repo's own docs and conventions say, with tests where \
the repo has them. {verify}

Leave your change in the checkout. Do not commit and do not push: MergeLoop commits it as one \
commit and pushes it.

End your last message with a line `{COMMIT} <subject>`, the commit's one-line subject, and \
before it a short description of the change for the pull request.

If the issue cannot be built as asked, because it is unclear, needs a decision a human must \
make, or needs access you do not have, stop and end your last message with a line \
`{GIVE_UP} <reason>`."
    )
}

/// The agent's description of its change for the PR: its last message without the lines
/// MergeLoop reads.
pub fn build_description(text: &str) -> String {
    text.lines()
        .filter(|l| {
            let l = l.trim();
            !l.starts_with(COMMIT) && !l.starts_with(GIVE_UP)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commit_subject, gave_up};

    fn req<'a>(verify: &'a [String], failed: Option<&'a VerifyFailed>) -> BuildRequest<'a> {
        BuildRequest {
            repo: "o/r",
            issue: 7,
            title: "Add a flag",
            body: "Add `--dry-run`.",
            verify,
            failed,
        }
    }

    #[test]
    fn the_prompt_names_the_issue_the_checks_and_the_markers_the_daemon_reads() {
        let verify = vec!["cargo test".to_owned()];
        let p = build_prompt(&req(&verify, None));
        for said in [
            "o/r",
            "Issue #7: Add a flag",
            "Add `--dry-run`.",
            "- `cargo test`",
            "Do not commit and do not push",
            "MERGELOOP_COMMIT: <subject>",
            "MERGEPATROL_GIVE_UP: <reason>",
        ] {
            assert!(p.contains(said), "{said}:\n{p}");
        }
        assert!(!p.contains("failed on it"), "{p}");
        // The markers the prompt asks for are the ones the daemon parses.
        assert_eq!(
            commit_subject("Adds it.\nMERGELOOP_COMMIT: Add a flag").as_deref(),
            Some("Add a flag")
        );
        assert_eq!(
            gave_up("MERGEPATROL_GIVE_UP: unclear").as_deref(),
            Some("unclear")
        );
    }

    #[test]
    fn a_second_run_is_told_what_failed() {
        let failed = VerifyFailed {
            command: "pnpm test".into(),
            output: "1 failed\n".into(),
        };
        let p = build_prompt(&req(&[], Some(&failed)));
        assert!(p.contains("`pnpm test` failed on it"), "{p}");
        assert!(p.contains("```\n1 failed\n```"), "{p}");
        assert!(p.contains("sets no verify commands"), "{p}");
    }

    #[test]
    fn the_description_leaves_out_the_markers() {
        assert_eq!(
            build_description("Adds the flag.\n\nMERGELOOP_COMMIT: Add a flag\n"),
            "Adds the flag."
        );
    }
}
