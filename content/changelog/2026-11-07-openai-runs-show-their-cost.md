---
title: OpenAI runs show what they cost
kind: improved
---

Runs on OpenAI's models now show their cost on the **History** page, next to Claude's, and count toward **LLM spend**. MergeLoop prices both from each vendor's list prices per token, including cached tokens and cache writes.

Runs from before today keep showing no cost. Runs on a ChatGPT plan show none either, since the plan bills no tokens. A run that sends a very long prompt can cost more than shown, because vendors charge more above a prompt length that MergeLoop cannot see per request.
