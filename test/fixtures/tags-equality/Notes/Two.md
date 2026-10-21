---
tags:
  - project
  - business-idea
status: active
---

TWO tags, one of them named `project`.

`tags == "#project"` is FALSE here, and so is `tags == ["#project"]`: the list
holds two elements, and a list only equals a scalar when it holds exactly one.
`tags.contains("project")` is TRUE regardless, because `contains` on a tag list
is a tag match rather than an equality.
