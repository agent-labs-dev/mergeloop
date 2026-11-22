---
title: A PR queued again after failed checks joins at once
kind: fixed
pr: 358
---

A PR that is queued again after its checks failed now joins the queue on the same pass, and an agent waiting on it is told to wait until it joins, not shown its earlier ejection. Stopping the daemon while a build's PR is opening keeps the build.
