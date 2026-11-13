---
title: Answer the agent's review in its threads on GitHub
kind: improved
---

When the agent's review asks a human about a finding, answer it in its thread on the PR. Resolve
the thread if the code is fine as it is, or reply with the change it needs, which goes to the
author. Approving the PR still answers every ask as fine, and requesting changes now sends them
all to the author.

To have the agent look again first, reply in the thread mentioning `@merge-loop`. It reads the
code again and answers in the thread. An author can do the same on a finding sent back to them:
if their reason holds, the agent withdraws the finding and resolves its thread.
