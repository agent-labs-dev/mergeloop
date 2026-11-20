---
title: A push stops the agent's work on the old commits
kind: improved
pr: 352
---

When you push to a queued pull request, MergeLoop stops the review, rebase or check fix it was running or waiting to run on the old commits, and starts on your new ones. You no longer wait behind work that would be thrown away, and a busy queue frees up sooner.
