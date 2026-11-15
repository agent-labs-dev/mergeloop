---
title: Authors can retry a failed automatic review
---

When an automatic review fails outside the pull request, its author can request another review of the exact current head. The retry is bounded and never answers findings or bypasses a human decision.

Transient failures while setting up an automatic review retry a bounded number of times. If setup still cannot start, the review finishes as an explicit human hold instead of remaining leased as running.
