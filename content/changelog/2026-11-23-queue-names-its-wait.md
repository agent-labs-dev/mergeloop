---
title: The queue names the checks it waits for
kind: fixed
pr: 360
---

A PR that waits on checks now says which required checks are still pending, so a wait with every check passed names the check that never reported. Queueing a PR also answers honestly: when the queue has yet to take it, it says the PR is waiting to join on the next tick instead of saying it is queued.
