---
title: A used up GitHub rate limit says when it resets
kind: fixed
---

When your GitHub token ran out of its rate limit, `mergeloop` and its MCP tools answered only
"Internal error." They now say the limit is used up and when it resets, and `mergeloop` waits
for the reset and carries on instead of exiting.
