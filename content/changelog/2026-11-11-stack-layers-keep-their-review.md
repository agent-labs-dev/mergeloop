---
title: A fix low in a stack no longer reviews every layer above it again
kind: fixed
---

The agent reviews each layer of a stack on what it changes over the layer below, and a layer whose own changes stay the same when the layers below move keeps its approval, so a deep stack stops waiting on the same reviews over and over.
