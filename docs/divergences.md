# Divergence registry

Every row is a deliberate, documented difference from Obsidian. None is a bug.

Each entry records the construct, our behaviour, Obsidian's behaviour, the
evidence, and whether a human must re-verify it by hand in the Obsidian UI.

**Obsidian version:** 1.13.7 (installer 1.12.4). Recorded in Phase 0. Parity
tests fail loudly on a version bump; that is the intended signal, not a flake.

Evidence shorthand: `probe` = a `.base` file written into `test/vault` and
queried with `obsidian base:query`, then deleted. `cli` = the same without a
helper base.

---

## D1 — `this` binding

| | |
|---|---|
| **Construct** | A base whose filters reference `this`, e.g. `project.contains(link(this.file.name))` |
| **Ours** | `this` binds to the Host note. A base referencing `this` with no host supplied is a **hard error**, never `[]`. |
| **Obsidian** | Binds `this` to whichever note the base is *displayed in* — the embedding file, the base file itself, or the active file in the sidebar. The CLI has no way to express that, so it returns `[]`. |
| **Evidence** | cli: `obsidian vault=vault base:query path=Tickets.base format=json` → `[]`. The same base resolves to two tickets when hosted in `Projects/SomeProject.md` and one when hosted in `Projects/OtherProject.md`. |
| **verify-in-obsidian** | no — the CLI evidence is decisive |

This is the one divergence the project exists to fix. See `CONTEXT.md`.

---

## D2 — `format=md` is a lossy export

| | |
|---|---|
| **Construct** | A view with `groupBy`, `summaries`, or a non-table `type`, rendered as markdown |
| **Ours** | Two surfaces. **`flat`** is byte-identical to the CLI: a single table for *every* view type, `groupBy` and `summaries` dropped, cells **centred**. **`structured`** is the agent-facing `base-rendered` Projection: `list` and `map` render as markdown lists, everything else as a markdown table, `groupBy` headers and a `summaries` footer kept, cells **left-aligned**. |
| **Obsidian** | Ignores `groupBy` and `summaries` entirely in `format=md`, emits the same flat table for every view type, and centres every cell. |
| **Evidence** | probe: a `list` view with `order: [file.name, status]` renders as a markdown *table*; a view with `groupBy: status` renders with no `**group**` header; a view with `summaries` renders with no footer row. |
| **verify-in-obsidian** | no |

The surfaces are split because "optionally a better md table than the CLI
returns" and byte parity cannot both hold for one output. On `flat` the
centring is load-bearing, not incidental: the surface is byte-compared against
`obsidian base:query format=md`, so left-aligning it would be a parity failure.
`structured` left-aligns deliberately — centred text is harder to scan, and
nothing diffs that surface against the CLI. The Projection is never written
back to disk — Obsidian would reject it, since a `base` fence must contain live
YAML — so it carries no parity obligation.

On `structured`, `table`, `cards`, `kanban`, `board`, `cardsTable` and any
unknown or plugin view type all render as the same markdown table, built from
the view's `order` columns. Only `list` and `map` earn a list. Asserted in
`test/unit/render.test.ts` and `test/parity/parity.test.ts`.

---

## D3 — `file.tasks` is an extension

| | |
|---|---|
| **Construct** | `file.tasks` |
| **Ours** | Implemented: task parsing over the note body, shape matching the `tasks` CLI. |
| **Obsidian** | Does not exist. Returns `null` for every row. |
| **Evidence** | probe: a view with `order: [file.tasks, file.tasks.length]` returns `"tasks": null, "tasks.length": null` on all rows, in both `format=json` and `format=md`. |
| **verify-in-obsidian** | no |

Phase 0 probed this specifically. It is absent in 1.13.7, so this is a logged
extension rather than parity. `Tickets.base` uses it in a formula, which is why
that base is a divergence case and not an oracle.

---

## D4 — Unimplemented constructs are hard errors

| | |
|---|---|
| **Construct** | Any expression we do not implement (unknown function, unknown namespace, unparsed grammar) |
| **Ours** | A hard error naming the construct. Never a silent `null`. |
| **Obsidian** | Obsidian reports an error *and* correct rows in the same result. |
| **Evidence** | Settled in Phase 0; Obsidian's dual reporting is why the plan calls for three-state result health (`ok` / `partial-with-errors` / `failed`) rather than imitating it. |
| **verify-in-obsidian** | yes |

---

## D5 — Unknown view types degrade to a table

| | |
|---|---|
| **Construct** | A view whose `type` we do not recognise, e.g. a plugin-supplied `gallery` |
| **Ours** | Render it as a markdown table using the view's `order` columns. No error. |
| **Obsidian** | Also renders it as a table in `format=md`; in the Obsidian UI the plugin supplies its own renderer. |
| **Evidence** | probe on 1.13.7: the CLI emitted a table for every view type tested. The view level is a documented open namespace — unknown view keys are "preserved verbatim and never interpreted" — and plugins write view types into it, so hard-failing a real vault on an unrecognised type was judged worse than a lossy table. |
| **verify-in-obsidian** | no |

This **reverses** the plan's original "unknown view type → hard error"
decision. Recorded here as a deliberate reversal, not an oversight.

The reversal is scoped to view **types** only. Unimplemented expression
constructs still hard-error, per D4, because a silently-null expression is far
more dangerous than a flattened table: you notice a table that lost a view, you
do not notice a formula quietly returning `null` on every row.

---

## D6 — A Projection round-trips silently, and never reaches disk

| | |
|---|---|
| **Construct** | Writing back a note obtained from `get_note`, where each Base region was handed back as a ` ```base-rendered ` fence carrying `path=`/`view=` |
| **Ours** | The rendered fence is recognised on the way back in, matched to the region it replaced by the Base path in its info string (not by position, so a reordered region still pairs correctly), and replaced by the stored region byte for byte. The fence never reaches disk. Writing an untouched Projection back is `health: ok` with no refusal. |
| **Obsidian** | No analogue — Obsidian has no agent-facing Projection. A `base-rendered` fence is inert to it: it would be stored as a dead copy of the rendered rows, and a `base` fence must contain live YAML to work at all. |
| **Evidence** | Verified end-to-end over a real MCP stdio session against `test/vault`: `get_note` then `write_note` with the content unchanged leaves the host note byte-identical, and the file on disk contains no `base-rendered` fence. Regression-pinned in `test/unit/project.test.ts`. |
| **verify-in-obsidian** | no |

The fence is replaced silently rather than refused, because round-tripping is
the DESIGNED flow: read the note, edit the prose, write it back. Reporting a
refusal on every round-trip would mark the happy path `partial-with-errors` and
teach an agent to ignore refusals, which costs more than it buys. The reconciler
could not draw the line anywhere else either — the rendered rows were never the
source of truth, so an untouched fence and an edited one are indistinguishable at
that point. The live region is restored byte for byte either way, so the agent's
row edits simply do not apply: the same outcome as any other unsupported
Base-region edit.

DELETING a region is still refused loudly — `health: partial-with-errors`, an
explicit "The base region was removed" refusal, and the region restored in
place. The difference is deliberate. A deletion is an unambiguous destructive
request, and an agent that removed a region has to learn that the write put it
back; a rendered fence is an artifact this server produced itself, so refusing
it would be refusing our own output.

---

## Behaviour probes that are NOT divergences

Recorded because they were non-obvious and cost real probing time. We match
Obsidian on all of these; each is pinned by a test so it cannot regress.

| Construct | Obsidian's behaviour | Why it looks wrong |
|---|---|---|
| `file.folder` at the vault root | `"/"`, not `""` | Every other root-ish value is empty. Probed with `Root Project.md` → `"folder": "/"` vs `Projects/SomeProject.md` → `"folder": "Projects"`. |
| Display labels | `file.name` → `file name`, but `file.folder` → `folder` and `file.properties` → `properties` | The namespace prefix is dropped for some file properties and kept for others. |
| Labels are not title-cased | `formula.priority_display` → `priority_display`, not `Priority Display` | Titles read like UI headers, so capitalisation looks expected. A configured `displayName` is also used verbatim (`PR Priority` stays as written). |
| `properties` keyed by a bare ID | Silently ignored | `properties: {status: {displayName: X}}` is dead config; only `note.status` matches. Verified with both spellings in one base. |
| Unsorted views | Ordered by `file.name`, not by path | Looks like "natural vault order". `test/vault` contains `Root Ticket.md` specifically so name order and path order disagree. |
| `file.basename` | Supported but **undocumented** | Absent from the official `file.*` table, yet live: labels as `file base name`. |
| JSON keys are labels | Keys are display labels, not Property IDs | `file.path` is emitted as `file path`; a formula can appear as `Priority` via `displayName`. |
| Exit codes | Obsidian prints `Error:` and still exits 0 | The exit code is meaningless; only output shape is trustworthy. |
| Ambiguous link resolution | Resolves to the **shortest path**, not a same-folder sibling | `[[Root Project]]` resolves to `Root Project.md`, not anything under `Projects/`. |
| `format=md` cell alignment | Every cell is **centred**, short values padded with spaces on both sides | Centred text is harder to scan than left-aligned, so it is tempting to "fix" — but `flat` is byte-compared against the CLI, so the centring is load-bearing. Only `structured` left-aligns. See D2. |

---

## Known limitations of `add_note_to_base`

`add_note_to_base` inverts a Base's filter into draft frontmatter, and inversion
is best-effort by nature: arbitrary expressions are not invertible, and the guess
that happens to verify is still a guess. The guarantee is not the inversion, it
is the verify step that follows it — which runs the base's real filter tree
against the draft's own frontmatter, in an overlay vault, before anything is
written.

Inverted today:

- `file.hasTag("x")` → `tags: [x]`
- `<prop>.contains(link(this.file.name))` with a `context` → `<prop>: ["[[<host basename>]]"]`
- `<prop>.contains(link("Literal"))` → `<prop>: ["[[Literal]]"]`
- `<prop> == "literal"` (either operand order; string, number or boolean)
- `and` groups, recursed
- note-property columns named in the view's `order`, seeded empty

Not inverted. Each becomes a clearly-marked `# TODO: could not invert ...`
placeholder in the draft, rather than being dropped:

- `or` and `not` groups, reported as `any of (...)` / `none of (...)` — which branch the author meant is a guess
- `!=`
- `file.*` comparisons (`file.ext == "md"`, `file.inFolder(...)`)
- `formula.*` predicates, bare or called
- `this.*` as the left-hand receiver
- string `contains` on a string receiver
- regex, date comparisons, lambdas
- two conjuncts that disagree about the same key (reported as a conflict)

Dropping an un-invertible filter would produce a draft that looks right and
matches nothing — the exact failure mode the handshake exists to prevent — so an
un-inverted filter is always surfaced rather than guessed at.

Two lifetime facts belong with the above. Drafts are process-local and lost on
restart, so a draft id that fails to resolve means starting the handshake again
without `draft_id`. A successful commit **consumes** the draft id; a failed
verify does not, so the same id is resent with the corrected content until it
expires.

## Bugs found in this implementation

Recorded separately from the divergences above, because these are defects we
repaired rather than differences we chose. Each was found by running the
TypeScript and Rust implementations against each other and disagreeing.

### `file.links` collapsed every link to one

`Vault.dedupe` keyed on `String(item)`. Every `LinkValue` stringifies to
`[object Object]`, so `file.links` returned a single element for any note with
two or more links. The docs say `file.links` is the "list of all internal links
in the note, including frontmatter", so returning one of three is wrong.

The cost was not confined to `file.links`: `backlinksFor` is built on
`linksFor`, so a note whose first link pointed elsewhere could lose a backlink
entirely.

Fixed in both implementations by keying on the value's own string form. A
repeated link is still deduplicated, which is what the original reached for.
Pinned by `file_links_keeps_every_distinct_link` on the Rust side and
`file.links keeps every distinct link` on the TypeScript side.

Not verified against the live Obsidian CLI: the bridge was down (empty stdout,
exit 0) while this was found and repaired. The reasoning is the spec sentence
above, and the fact that the pre-fix value was self-evidently wrong.

### An indented or blockquoted opening fence looped forever

`find_next_fence` scanned for ```` ``` ```` at a line start, handed the match to a
function that could reject it (blockquote prefix, backtick in the info string),
and then re-found the same line — with no forward progress. `parseNote` hung on
a note beginning with `  ```base` or `> ```ts`.

The Rust port requires strict progress. Behaviour is byte-identical everywhere
the TypeScript terminated, and it terminates where the TypeScript hung. Pinned
by `an_indented_fence_is_prose_rather_than_an_infinite_loop`.

### `Earliest` and `Latest` were advertised but unimplemented

The error message for an unknown summary name listed `Earliest` and `Latest`
among the supported values. Neither was in the switch, so using either threw.
Implemented in both.

### `parseConfig` validated `base` as if it were a note path

A refactor briefly routed the `base` argument through `assertNotePath`, which
refuses anything ending in `.base` — refusing the only valid value. Corrected
to check that `base` is present without asserting it is a note.

### The parity suite passed vacuously when Obsidian was down

Seven `if (!available) return;` guards made the suite report green with zero
assertions whenever the CLI was unreachable, which is the exact failure the
suite exists to catch. Replaced with `test.skipIf`, so absence is visible. The
same gap had a second cause: `available()` only checked the exit code, and
Obsidian exits 0 even with a dead bridge, so a dead bridge reported available.
It now requires non-empty stdout, and there is a `vaultReachable` check for the
case where the app runs but the vault is not open.

### A formula could not see the formula it referenced

Formulas were accumulated into a separate `formulaValues` object and assigned to
`ctx.formula` only after the evaluation loop. A formula reading
`formula.<other>` therefore saw `null`, and any arithmetic on it threw
`Expected a number but got null`.

The topological sort that orders the formulas was already correct, so the bug was
invisible in the ordering and only showed up in the values — which is why it
survived.

Confirmed against the TypeScript implementation before fixing: with
`formulas: {doubled: "formula.base_value * 2", base_value: "41"}`, the old code
throws and the fixed code yields 82.

Fixed in both implementations by writing each result into `ctx.formula` as it is
computed, so a later formula in the same note sees it. Pinned by
`a_formula_sees_the_formula_it_references` on both sides, over the shared
fixture at `test/fixtures/formula-chain`.

### An inline ```base fence could not round-trip

A base region written as an inline fence carries no `path=`, so the reconciler's
"same region" check — which required both sides to carry YAML — rejected it, and
the path-based fallback had no `path=` to match on. A clean `get_note` →
`write_note` on a note containing an inline fence reported two refusals and
replaced the live fence with nothing.

Fixed in the Rust port by pairing two inline regions on their pinned view alone.
The TypeScript tree still has this bug; it is not yet repaired there.

### `project` appended a spurious newline

The Projection builder added a newline after each rendered region unconditionally.
Because `write_note` reconciles rather than writes, that newline came back as an
edit the agent never made. Fixed by adding one only where the boundary does not
already have it.

### Date cells lost their `dateOnly` flag

`DateValue` carried a `dateOnly` flag, and `file.mtime` was built with it set, so
`toString()` truncated to `YYYY-MM-DD`. `BasesDate` keeps a fixed offset and
renders full RFC 3339.

This is a deliberate consequence of the value type rather than a defect: a
`dateOnly` flag on `BasesDate` would change its derived `Ord`, which the query
pipeline's sort depends on. The effect is that a rendered date cell in the Rust
port carries a time component the TypeScript dropped, which **will** show up in
`format=md` byte parity once Obsidian is reachable. Recorded here so it is not
discovered as a mystery later.

### `project` appended a stray newline (fixed in both)

The Projection builder added a newline after every rendered region without
checking whether the boundary already had one — and it almost always did, because
the prose following a region begins with the newline that ended the region's own
line. Since `write_note` reconciles rather than writes, that newline came back out
as a spurious edit to the host note, so a clean read-edit-write cycle was not a
no-op.

Fixed in both implementations. The round-trip is now byte-identical, and the
equivalence test asserts exactly that rather than comparing modulo trailing
whitespace.

### The draft commit's absence check consulted the index

`add_note_to_base`'s commit-time check read `Vault::note_paths()` — the index,
which only moves when this server writes. That is precisely the case the check was
written for and could not see: a human creates the note in Obsidian while reading
the proposal, Obsidian autosaves it, and no MCP call ever runs. `create_note` then
replaced the file verbatim and returned `verified: true`.

Reproduced end to end: propose a draft against `Tickets.base`, write the path
directly with `std::fs` (no server write in between), commit. Before the fix the
commit returned `Ok(Commit { written: …, verified: true })` and the note on disk was
the draft's `TODO: replace this paragraph` boilerplate. After, it returns
`… was created after this draft was proposed, so nothing was written` and the file
is byte-identical to what the human saved.

`VaultSource::exists` now answers from the storage rather than from a snapshot:
`tokio::fs::metadata` on the filesystem backend, a `Depth: 0` `PROPFIND` on WebDAV.
Over WebDAV **only a `404` means absent**; every other failure is an error, because
`false` is not a neutral answer there — it is permission to overwrite, so a `503`
turned into `false` would convert a connectivity problem into the silent loss this
check exists to prevent. `exists` is deliberately *not* in the `tolerated` table
for that reason: for `MKCOL` and `DELETE` a `404` is a success wearing a refusal's
clothes, where for `exists` it is the answer.

The propose-time check still reads the index, on purpose. Nothing has been written
at that point, so a stale listing costs a false refusal and nothing else — and a
false refusal is the safe direction, because it stops a draft rather than permitting
a write. Only the last check before an overwrite has to be true at the instant of
the write.

The TypeScript tree already asks the backend here and the Rust port regressed it,
so the port is the second implementation to need the fix rather than the first.
One place is deliberately NOT equivalent: the TypeScript filesystem backend
answers `false` for every `fs.stat` throw, because `fs.stat` hands back a
`SystemError` whose `code` the `catch` never reads — so a permissions error
reports the note as absent and the commit proceeds. `FsVaultSource::exists` here
distinguishes `NotFound` from every other code and refuses the rest. `false` in
this method is permission to destroy a note rather than an absence of
information, so the looser reading is the one that loses data.

### A restored Base region was spliced into a CRLF Host note with a bare LF

`push_line` appended `\n` to the joins it generates, so a deleted region restored
into a CRLF Host note came back as `…\n` between two CRLF lines — a mixed-ending
file, which is what the CRLF work in `src/note.rs` (c4af62f) existed to prevent. The
region's own bytes come from the original and carry whatever it used, but an inline
` ```base ` fence spans no terminator at all and an embed at end of file spans none
either, so the byte after a restored region is always one `push_line` chose. At that
point an embed whose own line ended CRLF appeared immune, because its span swallowed
the `\r` and `push_line`'s bare `\n` completed the pair. It was not immune; it was
wrong in the other direction, which is the next entry.

`reconcile_note` now detects the note's line ending once, from the first terminator
in the original, and writes both joins with it. First, deliberately: `write_note`
re-reads the note on every call, so a rule that depended on where the note was
edited from — the terminator beside the restore point, the majority, the last one —
could pick a different answer on the next write and rewrite the file's endings
underneath the user. A note with no terminator gets `\n`: it has no convention to
honour, and the region still has to be terminated.

The TypeScript tree fixed this first and the Rust port regressed it, so the two
now differ in one documented place: the tree asks whether a `\r\n` appears
*anywhere*, this one asks which terminator comes *first*. They disagree only on a
note that is already mixed, and most often on one Windows paste inside an
otherwise LF note — where "anywhere" picks CRLF for a region restored beside LF
prose. Recorded here rather than left as a silent difference between two trees that
are meant to be equivalent.

### A restored Base region came back as `\r\r\n`, and then vanished

A CRLF Host note whose Base region sat on a **terminated** line came back from
`write_note` with a doubled carriage return where the region's terminator used to
be, and lost the region on the very next read.

The region's byte span included the `\r`. Rust's `(?m)` treats only `\n` as a line
terminator, so `$` does not match before a `\r` and the pattern's trailing `\r?`
has to CONSUME it to reach `$`; JavaScript's multiline `$` matches there, so the
TypeScript tree's span stopped short and the `\r` belonged to the prose after it.
A span that already ends in `\r` plus the `\r\n` `push_line` appends is `\r\r\n`.

Nothing reported it. The write was `health: partial-with-errors`, the embed was
still plainly visible in the file, and a doubled carriage return is a line ending
every editor silently rewrites — so nothing would ever have noticed except a human
comparing bytes much later. What it destroyed was the region: `![[T.base]]\r\r`
cannot satisfy `[ \t]*\r?$`, so the next read answered `regions: []` with no
refusal at all, and the agent's next deletion of that invisible region reported
`removedRegion: false`, `refusals: []`, `health: ok` — and was written. The first
write refused a deletion and the second one silently performed it.

Only the terminated-line shape reaches it. A Base region at end of file has no
terminator for the join to double, which is why the two corpus hosts that end with
their embed never showed it and every existing test passed.

Two things hid it. The assertion that watched for mixed endings counted `\n` not
preceded by `\r`, and `\r\r\n` contains no lone `\n` — it looked straight through
the corruption. And a test comment already named the span as "a separate question
about `split_base_embeds`" rather than as a bug, which is how a known-wrong span
became a load-bearing one.

Fixed in the span rather than in the pattern, because the `regex` crate has no
look-around to say "…but not that `\r`": `split_base_embeds` trims the match back to
where JavaScript's ended, which also makes the two trees byte-identical here
instead of merely equivalent. `ends_its_own_line` now reads a leading `\r\n` as the
line boundary it is, so the Projection of a CRLF note no longer splices a bare LF
after a region either.

Pinned on all three surfaces: the span against the exact bytes JavaScript produces
(`expected_region` in `tests/note.rs`), every restore shape for both bare LFs and
doubled carriage returns plus a re-parse (`tests/render.rs`), and two consecutive
deletions through the tool surface, because the first corrupts the file and the
second loses the region — asserting only the first cannot tell a repaired restore
from a slightly wrong one (`tests/tools.rs`).

## TypeScript and Rust: verified equivalent

Both implementations are live and both pass their own suites. They were also run
side by side through a real MCP stdio session, eleven cases across all six tools,
and compared as parsed JSON (key order in a JSON object is not meaningful):

| | |
|---|---|
| Equivalent (key order ignored) | **11 / 11** |
| Byte-identical text blocks | 6 / 11 |

The five non-byte-identical cases differ only in the key order of the
human-readable ```` ```json ```` text block: the TypeScript puts `rows` first,
Rust puts the metadata first. `structuredContent` — what a machine reads — is
identical in every case, and `format=markdown` is byte-identical, which is the
surface the parity suite compares against the Obsidian CLI.

`serde_json`'s `preserve_order` feature is enabled so insertion order is kept
rather than sorted; the remaining difference is the order the payload is built
in, which is a presentation choice with no bearing on the result.

## The Rust parity harness fails loudly rather than skipping

Rust's test harness has no `skip`. A test that returns early reports `ok`, which
is the vacuous-green failure this project had already suffered once in
TypeScript — seven `if (!available) return;` guards, and a suite that reported
green with zero assertions whenever Obsidian was unreachable.

`tests/parity.rs` therefore **panics** when the CLI cannot answer, and only
relaxes when `BASES_MCP_ALLOW_SKIP=1` is set explicitly:

```
$ cargo test --features parity --test parity
test result: FAILED. 5 passed; 3 failed

$ BASES_MCP_ALLOW_SKIP=1 cargo test --features parity --test parity
[parity] Obsidian CLI unavailable — COMPARING NOTHING this run.
test result: ok. 8 passed
```

"Compared nothing" is therefore loud by default and deliberate to opt into, which
is the opposite of the original bug. The target is behind a Cargo feature
(`required-features = ["parity"]`), so a plain `cargo test` never builds it and the
`--features parity` above is what puts it back — see `docs/tooling.md`.

The availability probe is a real `base:query` that must return parseable rows,
not `obsidian version` and not a non-empty check. A half-started bridge answers
`version` and then returns an empty string for every query, so a weaker probe
passes while nothing can be compared.

Note that Obsidian's exit code is not a signal: it exits 0 with empty stdout
when its bridge is down. Both harnesses therefore require non-empty output, and
`tests/parity.rs` pins that with a test that runs the availability check against a
binary that exits 0 silently (`true`) and asserts it reads as UNAVAILABLE — the
guard, guarded.
