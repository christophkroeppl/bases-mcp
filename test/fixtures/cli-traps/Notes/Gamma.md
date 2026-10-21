---
kind: index
status: open
---

The note with NO `rank`. Its `note.rank` cell is null, which is what makes
the "nulls sort last" trap observable: in both directions this row comes
after the rows that have a rank.
