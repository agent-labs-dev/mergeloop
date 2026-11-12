---
title: MergeLoop approves a PR on GitHub once its review passes
kind: improved
pr: 315
---

When the agent's review finds nothing left to change, or you resolve everything it sent back, MergeLoop now approves the PR on GitHub instead of only dismissing its request for changes. Its approval counts toward your branch's required reviews, unless your rules ask for a human's.

If your rules dismiss stale approvals, MergeLoop approves again once it has reviewed the new head. It never approves a PR it opened itself.
