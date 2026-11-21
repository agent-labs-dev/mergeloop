# MergeLoop

A merge queue for GitHub, with an agent on conflicts, review and failed checks. `CONTRIBUTING.md` says how to build,
test and open a PR.

## Gate

`scripts/gate` runs rustfmt, clippy and the dashboard's biome check; `scripts/gate --changed` lints only the crates and
the dashboard that the branch changed. `cargo test --workspace` needs the Postgres from `compose.dev.yml`, so CI runs it.

## The `mergeloop` agent skill

`crates/web/src/skill.md` is the one source of the skill MergeLoop installs in other repos. `.agents/skills/mergeloop/SKILL.md`
and `.claude/skills/mergeloop/SKILL.md` are byte-identical copies, and a test in `crates/web/src/api.rs` fails when they
drift: change all three together. Other repos install it as `README.md` says under "Install the agent skill by hand".

## Agents can do what the dashboard does

The dashboard does nothing but call the JSON API under `/api`. Every route is in `ROUTES` in `crates/web/src/routes.rs`, and the MCP server at `/mcp` serves one tool per route, which calls that route as the agent. So the API and MCP always match the UI, as long as each change keeps to this:

- A new dashboard feature gets a route in `ROUTES` with a `tool!`, never a `.route(` in `lib.rs`. Exempt a route only when no agent could use it, such as a browser sign-in or a webhook, and give the reason. The tests refuse an exempt `/api/w/` route.
- A changed route gets its tool's description and `args` updated to match.
- `README.md` lists the tools between `<!-- tools` markers, in the order of `ROUTES`. A test checks it.
- Auth comes from the `Caller` extractor in `crates/web/src/auth.rs`: the dashboard's session, or a GitHub token in `Authorization: Bearer`. Call `caller.repo(...)` for a call on one repo, which a GitHub token may make. Call `caller.workspace(...)` for a call across a workspace, which needs a sign-in. Act on GitHub with `caller.github_token(...)`, so GitHub checks what the caller may do.

## Changes users see go in the changelog

A PR whose change MergeLoop's users can see or feel adds an entry to `content/changelog/`, with a picture from the PR's screenshots when there is one to show. `content/changelog/README.md` says when and how.

## Standards

- `repo-standards`: the RS-* requirements every cjber and agent-labs-dev repo meets; its `audit.py` checks this repo.
