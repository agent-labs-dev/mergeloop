---
title: MergeLoop no longer approves a PR right after requesting changes on it
kind: fixed
pr: 318
---

A review that sent comments back could be followed seconds later by MergeLoop's approval, before the author had resolved them. Now MergeLoop approves only once every comment is resolved.
