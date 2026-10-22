---
tags: []
status: active
---

An EMPTY tag list. `tags.contains("project")` is FALSE here, not null: the
property is present and holds no tags. Measured against Obsidian 1.14.4.

A note with NO `tags` property at all is a different case -- there the
property is null, and a null receiver is its own divergence.
