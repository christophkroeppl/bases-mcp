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
