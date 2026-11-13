# The ruleset bypass for built PRs: what GitHub documents

The RFC's third row (agent-labs-dev/mergeloop#316, open question 2): the maintainers make MergeLoop's App a bypass actor on the ruleset that asks every PR for one approval, keep the paths that must never merge dark in a second ruleset with no bypass, and MergeLoop's own verdict stands in for the approval on a PR it built.

MergeLoop does not do this yet. GitHub's documentation, read on 2026-11-13, leaves two things open that the design stands on. This file says what it does settle, what it leaves open, and the one check on a scratch repo that settles the rest.

## What the documentation settles

**An App can be a bypass actor.** A ruleset's bypass list takes repository roles, teams, GitHub Apps and Dependabot ([Granting bypass permissions](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/creating-rulesets-for-a-repository#granting-bypass-permissions-for-your-branch-or-tag-ruleset)). Through the API, an entry is `{ "actor_type": "Integration", "actor_id": <App id>, "bypass_mode": ... }` ([Create a repository ruleset](https://docs.github.com/en/rest/repos/rules#create-a-repository-ruleset)).

**Three bypass modes.** `always`; `pull_request`, which the UI calls "For pull requests only": the actor may bypass the rules only on pull requests, on branch rulesets; and `exempt`: the rules do not run for the actor at all, and no bypass is recorded in the audit log. The same page.

**A bypass is used on a merge only when asked for.** `PUT /repos/{owner}/{repo}/pulls/{pull_number}/merge-async` takes `bypass_rules`, "whether to bypass repository rules that the authenticated actor is permitted to bypass", false by default ([Merge a pull request asynchronously](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request-asynchronously)). The queue merges through that endpoint and never sends `bypass_rules` (`a_merge_never_asks_github_to_bypass_its_rules` in `crates/github/src/repo.rs`). The synchronous `PUT .../merge` has no such field ([Merge a pull request](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request)).

**Rulesets layer.** All rulesets that target a branch apply together. When two define the same rule differently, "the most restrictive version of the rule applies" ([About rule layering](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/about-rulesets#about-rule-layering)).

**What the App can read about a branch's rules.** Both with the "Metadata: read" permission every App has ([Permissions required for GitHub Apps](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)):

- `GET /repos/{owner}/{repo}/rules/branches/{branch}`: each active rule on the branch, with the `ruleset_id` it comes from ([Get rules for a branch](https://docs.github.com/en/rest/repos/rules#get-rules-for-a-branch)). The queue reads it today for status checks and merge methods.
- `GET /repos/{owner}/{repo}/rulesets`: each ruleset with `current_user_can_bypass`, one of `always`, `pull_requests_only`, `never`, `exempt`, "the bypass type of the user making the API request" ([Get all repository rulesets](https://docs.github.com/en/rest/repos/rules#get-all-repository-rulesets)). The bypass list itself is returned only to a caller with write access to the ruleset.

**What GitHub says about a PR that cannot merge.** Nothing that names the rule:

- REST `mergeable_state` is a string with no documented values ([Get a pull request](https://docs.github.com/en/rest/pulls/pulls#get-a-pull-request)).
- GraphQL `mergeStateStatus: BLOCKED` is "The merge is blocked" ([MergeStateStatus](https://docs.github.com/en/graphql/reference/enums#mergestatestatus)).
- GraphQL `reviewDecision` is one answer for the PR, across every ruleset ([PullRequest](https://docs.github.com/en/graphql/reference/objects#pullrequest)).
- GraphQL `viewerCanMergeAsAdmin` "indicates whether the viewer can bypass branch protections and merge the pull request immediately" (same page).
- Rule suites say which rule failed or was bypassed, but only for a push or merge that already happened, and they need "Administration: read" ([Rule suites](https://docs.github.com/en/rest/repos/rule-suites)).

## What it leaves open

1. **Whether a bypass on one ruleset lifts a rule a second ruleset also defines.** The design's two rulesets both hold a `pull_request` rule: one approval in the first, a code owner's review in the second. Layering merges them into the most restrictive version. Whether the bypass that `bypass_rules` uses is then judged per ruleset, so the second still holds, or on the merged rule, is not documented. If it is the merged rule, the sensitive paths merge dark.
2. **Whether the server can tell, before it merges, that a PR is blocked only by a rule MergeLoop may bypass.** No documented field names what blocks a PR. Two would serve, but their documentation does not say they do: `viewerCanMergeAsAdmin` for an App installation (is the installation the viewer, and does it count a ruleset it cannot bypass?), and `current_user_can_bypass` read with an installation token (is the installation the "user"?). Without them the server can only reason from the rules it reads, and a bypass that guesses is worse than none.

Until both are settled MergeLoop has no bypass setting, and no merge sends `bypass_rules`.

## The check on a scratch repo that settles them

Read-only for MergeLoop: no code changes, a scratch repo in an organization, an App installed on it (MergeLoop's own, or one made as the README says).

1. Add a `CODEOWNERS` file that gives `sensitive/` to a team or user other than the App.
2. Ruleset **A**, targeting the default branch: a `pull_request` rule with 1 required approval. Bypass list: the App, "For pull requests only".
3. Ruleset **B**, same target: a `pull_request` rule with "Require review from Code Owners", 0 required approvals. No bypass list.
4. As the App, open two PRs: **P1** changes `ok.txt`; **P2** changes `sensitive/x.txt`. Approve neither.
5. With the installation's token, record:
   - `GET /repos/{o}/{r}/rulesets`: `current_user_can_bypass` of A and of B.
   - For P1 and P2, in GraphQL: `mergeStateStatus`, `reviewDecision`, `viewerCanMergeAsAdmin`.
6. For each PR, `PUT /repos/{o}/{r}/pulls/{n}/merge-async` with `{"merge_action":"direct_merge","bypass_rules":true}`, then poll `GET .../merge-async/{uuid}` until it has finished.

What makes the bypass safe to build: P1 merges and P2 fails (open question 1); and some field from step 5 differs between P1 and P2, so the server can tell them apart before it merges (open question 2). The expected answer is A `pull_requests_only`, B `never`, and `viewerCanMergeAsAdmin` true on P1 and false on P2. If P2 merges, the design in the RFC does not hold and the paths that must never merge dark need a protection that is not a ruleset rule.
