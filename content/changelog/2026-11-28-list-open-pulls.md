---
title: Agents see every open PR, queued or not
kind: new
---

The `list_open_pulls` tool, and `GET /api/w/{id}/pulls`, list the open PRs from the moment they open: drafts, PRs in review and PRs in the queue. Each shows its author, base branch, labels, reviews and checks, and where the queue has it, so an agent can track all of a workspace's PRs through MergeLoop alone. Pass `author` for one person's PRs, or `repo` for one repo's.
