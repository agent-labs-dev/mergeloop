---
title: Agents watch a queued PR until it merges or is ejected
kind: improved
---

The agent skill MergeLoop installs now tells the agent that pushed or queued a PR to keep watching it until it merges or MergeLoop ejects it. Before, it could stop once the review threads were resolved or the PR held on a human, and miss a later review, fix or ejection. An agent that has to stop earlier now says the PR is still queued and unwatched. Update the skill from **Repos** to pick it up.
