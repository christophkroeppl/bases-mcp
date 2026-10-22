---
tags:
  - ticket
project:
  - "[[Root Project]]"
status: active
priority: high
type: task
---

# Root ticket

A ticket at the vault root rather than under `Tickets/`. Its `project` link is a
bare `[[Root Project]]`, which is ambiguous on its face — the basename also
matches the nested `Projects/` hosts — so it exercises the shortest-path rule:
`Root Project.md` wins over `Projects/Root Project.md`.

- [ ] check the root-level path round-trips
