---
title: The CLI says why a repo's queue does not run
kind: fixed
---

When `mergeloop` finds a repo whose queue does not run, its brief names the one cause, such as the
App not being installed, no config file on the default branch, or the dashboard turning it off,
and the one thing that fixes it. It exits 12, waiting on a human, instead of 2, which means an
error.
