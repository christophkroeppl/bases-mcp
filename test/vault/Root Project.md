---
tags:
  - business-idea
  - project
type:
  - "[[Geschäftsidee]]"
categories:
  - "[[Projects]]"
status: active
priority: high
description: Eine Projektnotiz direkt im Vault-Root, ohne Ordner.
---

## Tickets

A root-level host note. `file.folder` is `"/"` here, not the empty string —
Obsidian reports the vault root that way — and `this.file.name` resolves to
`Root Project.md`, so the embedded base must scope itself to this note just like
the two hosts under `Projects/` do.

![[Tickets.base]]

## Inline

The same scoping, expressed with an inline fence instead of an embed. Obsidian
binds `this` to the containing note, so this resolves identically.

```base
filters:
  and:
    - file.hasTag("ticket")
    - project.contains(link(this.file.name))
views:
  - type: list
    name: Inline
    order:
      - file.name
      - status
```
