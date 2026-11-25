---
title: A rebase keeps the review's approval, and new commits get a review of their own
kind: improved
---

When you rebase a queued PR that MergeLoop's review approved, and its changes stay the same, the approval holds and the PR keeps its place. There's no second review to wait for.

When you push new commits on top of an approved version, rebased or not, the review reads only those commits, so it finishes sooner. A push that changes commits the review already approved, such as an amend, a squash or a resolved conflict, still gets a full review.
