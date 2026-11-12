---
title: See each PR of a stack in the queue
kind: improved
image: /changelog/stack-in-queue/stack.png
---

A stack in the queue now lists its PRs, top first, down to the branch it merges into: each with its title, checks and diff. The PR that holds the stack shows what holds it, such as the agent's question for a human, and the PRs below it show they passed review while those above wait for it.

A long stack folds runs of alike PRs into one line, such as "Layers 1–13 · 13 PRs approved". Click it to show their titles.
