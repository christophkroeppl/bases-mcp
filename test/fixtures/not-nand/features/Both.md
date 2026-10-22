---
kind: feature
also: advanced
---

In BOTH `features` and `advanced`. Still excluded: NAND excludes on ANY match,
so this distinguishes it from a naive `!(a || b) && !(c || d)` reading only if
someone misreads "not" as "all must be false in combination".