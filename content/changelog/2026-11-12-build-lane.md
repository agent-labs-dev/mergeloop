---
title: Label an issue, and a developer's machine builds it into a merged PR
kind: new
pr: 317
image: /changelog/build-lane/builds.png
---

Turn on **Build lane** in Settings, put your repo's label on an issue, and run `mergeloop login` and `mergeloop daemon` on your machine. The daemon has the coding agent you already use build the issue, a second agent review it, and MergeLoop opens its PR, which the daemon lands through the queue. The new **Builds** page shows each build, who holds it, and how far it is.

Builds run on your own machine with your own agent login, and by default may write only a folder of their own. The docs say what an agent can still read and reach there, so read them before you turn it on.

Only developers GitHub lets push to a repo build its issues. MergeLoop never approves a PR it built: your branch rules decide whether one needs a human.
