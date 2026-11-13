---
title: A sign-in from an app or the terminal asks you first
kind: improved
image: /changelog/sign-ins-ask-first/consent.png
---

When an MCP client such as Claude Code, or `mergeloop login`, signs in to MergeLoop, MergeLoop now
shows a page that says which program asks and what allowing it gives it, with **Allow** and
**Deny**. Nothing signs in as you until you allow it.

`mergeloop login` prints a short code, and the page shows the code the sign-in carries: allow it
only when the two match. A sign-in with no code says so.
