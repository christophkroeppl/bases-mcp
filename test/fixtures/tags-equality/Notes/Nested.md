---
tags:
  - project/alpha
status: active
---

A NESTED tag. `tags.contains("project")` is TRUE because the tag match lands
on a `/` boundary, and `file.hasTag("project/alpha")` is TRUE for the same
reason. `tags.contains("proj")` is FALSE: the match is a prefix on the TAG,
not on the needle.
