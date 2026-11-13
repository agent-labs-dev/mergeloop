---
title: A PR the build lane opened counts for the developer who built it
kind: improved
---

GitHub shows the MergeLoop App as the author of every PR the build lane opens. MergeLoop now
treats such a PR as the PR of the developer whose daemon holds its build: on a private repo it
takes that developer's seat when it joins the queue, as a PR they opened would, and the history
and the people view list it under their name. A developer still has one seat, however many of
their PRs are built.
