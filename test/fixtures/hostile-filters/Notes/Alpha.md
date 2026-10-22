---
kind: note
when: 2026-05-26
tags:
  - fixture-tag
---

A plain note. `needs_follow_up` is true for kind "note".

The tag is `fixture-tag`, not `ticket`, so that this note cannot leak into the
realistic `Tickets.base` at the vault root, which selects on `file.hasTag("ticket")`.