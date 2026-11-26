---
title: A failed automatic review says it failed
kind: fixed
---

A pull request whose automatic review fails is held as **Review failed**, not as a review that asks a human questions. Its status, the `mergeloop` CLI and the PR comment say the review failed and why, and that its author can retry it or a human can review it instead. Reviews of pull requests that change binary files, such as gzip fixtures, now save.

A pull request labelled to merge in a repository whose queue is off gets a comment saying why it is not queued.
