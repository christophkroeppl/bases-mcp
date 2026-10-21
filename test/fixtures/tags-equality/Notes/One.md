---
tags:
  - project
status: active
---

ONE tag, named `project`.

The bare-name equality `tags == "project"` is FALSE in Obsidian, because a tag
list is never loose-equal to a bare string. But `tags == "#project"` is TRUE
here, because a list compares equal to a scalar only when it holds exactly one
element, and that element is the tag whose text is `#project`.
