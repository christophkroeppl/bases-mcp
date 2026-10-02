# Divergence registry

Every row is a deliberate, documented difference from Obsidian. None is a bug.

Each entry records the construct, our behaviour, Obsidian's behaviour, the
evidence, and whether a human must re-verify it by hand in the Obsidian UI.

**Obsidian version:** 1.13.7 (installer 1.12.4). Recorded in Phase 0.

⚠️ **The version gate described here does not exist yet.** Every parity result
below was measured against 1.13.7, and nothing in `tests/parity.rs` asserts the
version — it reads no version at all. So an Obsidian upgrade would silently
re-baseline every claim in this file rather than failing loudly. Closing that
means recording the CLI version into the parity snapshot and asserting it; until
then, treat the version above as "the version these entries were measured
against", not as a check.

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

## D7 — A Base deeper than 96 nesting levels is refused

| | |
|---|---|
| **Construct** | A filter or formula nesting groups more than 96 levels deep, e.g. `((((…status != null…))))` |
| **Ours** | A hard error naming the construct and the limit. Nothing is written. |
| **Obsidian** | Renders the Base. A stack overflow is not catchable in Rust, so the alternative to a limit is a process abort: the MCP client sees the transport drop with no tool result, and every tool stays dead until the server restarts. |
| **Evidence** | Measured on `develop` at `f377887`: 2500 levels evaluated, 3000 evaluated, 4000 → `fatal runtime error: stack overflow, aborting`, exit 134. After the guard: 96 levels is a refusal naming `Expression` and the number; 4000 is the same refusal, exit 0. |
| **verify-in-obsidian** | no — the crash is ours, not Obsidian's |

The parser, the evaluator and the three filter-tree walks spend from one shared
budget (`src/depth.rs`). **Why 96.** The parser is the binding constraint: it
survives 472 and dies at 473 at `opt-level = 0` in a 2 MiB thread, so 96 sits
4.9x below that edge with the stack mostly unspent — the margin that matters,
because a frame that grew by half would otherwise move the edge to 314. It is
also roughly 2x deeper than anything a person writes; the deepest legitimate
expression this project has been asked to accept is fifty. Deliberately **not**
64: `serde_yaml` already caps a filter tree at 63, so a filter may nest `and:`
63 deep *and* carry a 96-deep expression, and a shared limit would refuse a Base
for no reason.

**Obsidian will never produce one of these notes.** It writes LF on every
platform (see below), so a nested expression arrives only from a hand edit, a
plugin, or a bad merge — which is exactly the shape a limit is for.

---

## D8 — CRLF is normalised to LF on read

| | |
|---|---|
| **Construct** | A note whose bytes on disk are CRLF, read through this server |
| **Ours** | `VaultSource::read_note` and `read_fresh` replace `\r\n` with `\n` before anything above the backend sees the note. A lone `\r` is left alone. Every read, every cache entry and every `content_hash` taken above the backend is therefore over LF. |
| **Obsidian** | Does not normalise at read time. It normalises on OPEN: opening a CRLF note converts it to LF in the app, and the conversion reaches disk the next time the note is saved. Until then the bytes stay CRLF, and so do any other reader's. |
| **Evidence** | The five sources under "Obsidian writes LF on every platform", including the Obsidian staff reply that there is no way to change this. Our side is measured by `the_two_spellings_of_a_note_get_the_same_base_hash` and `a_base_hash_from_a_crlf_note_round_trips_through_an_obsidian_resave` in `tests/service.rs`. |
| **verify-in-obsidian** | no — the staff reply is decisive on the direction, and the open-time conversion is reported from three independent threads |

**Where it is observable.** In `get_note`'s `raw`, and only there. An agent asking
for a CRLF note gets LF back, and the bytes on disk do not match what it was
handed. Everything else agrees: `get_note`'s `content` is built from the same
normalised text, so the Projection of a CRLF note is byte-identical to the
Projection of its LF twin. And `write_note` writes back what the reconciler
produced, which is built from a normalised read, so an edited CRLF note comes back
LF — Obsidian's own answer for the same file, arrived at a moment earlier.

**Why at the backend rather than in `Vault`.** `Vault` has one `read_note` that
delegates, so normalising there would be one line in the right place — and one
line a future caller could route around, because `service.rs` calls
`self.backend().read_fresh` directly in two places and will do so again. At the
trait, no read in the process can avoid it. The trait method is named `read_note`
rather than `read_text` because it does not return the text on disk, and a
function that translates is not doing what its name says.

**Why `\r\n`, and never a bare `\r`.** `note::Lines` finds `\n` and nothing else,
so a CR-only note is ONE line to this crate. Replacing every `\r` would make it
one line per carriage return, inventing a line count it never had, and would stop
it round-tripping byte for byte. Leaving a lone `\r` alone keeps it exact.
`\r\r\n` becomes `\r\n`, because the pair at the end of it is still a pair and
degrading a doubled carriage return on the way to being normalised is strictly
better than passing it up.

**What it buys.** `content_hash` is taken over normalised text, so a CRLF note and
its LF twin hash alike. Two classes of failure go with that:

- **The refusal that was not a refusal.** Obsidian converts CRLF to LF when it
  opens a note, so the bytes behind a note can turn over between an agent's read
  and its write with no human involved. `write_note`'s `base_hash` check compares
  against a fresh read, so that turn used to be reported as "changed since you
  read it" — an error telling the agent to re-read and retry, which lands on the
  same answer for as long as the two spellings disagree.
- **The rewrite of an untouched note.** Prose differing only in its line endings
  is byte-identical once normalised, so a clean `get_note` → `write_note` round
  trip produces `changed: false` and the file is not touched. Measured: a CRLF Host
  note put through a full Projection round trip goes 48 bytes on disk before and 48
  bytes after, still CRLF. Without normalisation the same round trip rewrites it —
  48 bytes to 49, a bare LF spliced in after the Base region and one extra blank
  line — which is the corruption recorded under "Bugs found in this implementation"
  reached through a path nobody was watching. **Measured, not pinned by a test:** it
  was observed with a throwaway harness rather than added as one, because the shape
  tests this replaced are gone and a round-trip-no-op assertion for LF already
  exists (`an_edit_that_changes_nothing_does_not_touch_the_file`).

**What it does not buy, and what must stay true elsewhere.** `content_hash` itself
normalises nothing: it is a pure function of its argument, because the WebDAV
write verification hashes the bytes it PUT against the bytes the server stored,
and a hash that folded CRLF would stop being able to see a server that rewrites
endings. Every caller above the backend is handed already-normalised text, and
that obligation is pinned by a test.

The PARSER is also still total over CRLF, deliberately. `base_embed_re` still
consumes a `\r` before the line end and `split_base_embeds` still trims it back,
because `write_note` parses what the agent sent — text that never passed through
the backend. An agent whose editor rewrote a Base region's line ending would
otherwise have the region fail to match, the reconciler read it as deleted, and
the restore — which anchors on the surviving regions and finds none — append the
region to the end of the Host note. A parser that cannot see a region cannot
protect it.

**The TypeScript tree differs here.** `ts-implementation` preserves line endings:
`line_ending` there and here both honoured the note, and both are gone. A CRLF note
round-trips through that tree with its endings intact and comes back from this one
as LF. This is the one place the two implementations are knowingly not equivalent,
and the registry is where that belongs rather than a comment in either tree.

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
| **Notes are always LF** | Obsidian writes `\n` on every platform, and converts a note's CRLF back to LF when it opens it | It looks like a bug when a Windows vault is full of CRLF and Obsidian "changed" every file on open. It is Obsidian doing the normalising, not the server. |


### Obsidian writes LF on every platform

This is the load-bearing fact behind the CRLF handling in `src/note.rs`, so it is
recorded with sources rather than left as folklore.

| Platform | What Obsidian writes | What it does to a CRLF note it opens |
|---|---|---|
| Windows | `\n` | Converts back to `\n` |
| macOS | `\n` | — |
| Linux | `\n` | — |

There is **no setting**. Sources:

- [Changing the storage format of a Markdown file from unix- to windows format](https://forum.obsidian.md/t/changing-the-storage-format-of-a-markdown-file-from-unix-to-windows-format/79822) (Apr 2024). Question: *"The files Obsidian produces use the `\n` instead of `\r\n`. I'm on a Windows machine and I would like the default line ending to be `\r\n`. I've looked through the options but I can't find anything that controls it."* Obsidian staff: *"There's no way in the app itself."*
- [Can I select CRLF line endings?](https://forum.obsidian.md/t/can-i-select-crlf-line-endings/5206) (2020): *"When I create new notes they are created with CRLF (I have Windows 10, Obsidian v 0.8.9) but when I start writing on them they turn LF."*
- [Being able to select line endings CRLF - LF](https://forum.obsidian.md/t/being-able-to-select-line-endings-crlf-lf/5294) (2020), a pinned feature request that stays open.
- [Obsidian crashes in very specific circumstances due to LF vs CRLF](https://forum.obsidian.md/t/obsidian-crashes-in-very-specific-circumstances-due-to-lf-vs-crlf/85190) (Jul 2024): *"When re-opening the note, Obsidian should convert CRLF → LF like it usually does."*
- [Solving Git Sync Issues Caused by Different System Line Endings](https://forum.obsidian.md/t/solving-git-sync-issues-caused-by-different-system-line-endings/92253) (Nov 2024): *"Apple and Linux use LF, and Obsidian's default line ending is also LF."*

**What this means for us.** CRLF is the FOREIGN shape, not the native one. It
arrives from `core.autocrlf=true` on a Windows checkout, a sync client that
forces CRLF, or editing outside Obsidian — never from Obsidian. What we do about
it is D8, and the short version is that we stop carrying the question: a note read
through this server is LF above the backend, always.

Two consequences are worth stating, because both were learned the hard way and
one of them no longer applies:

1. **We must not write CRLF ourselves.** Obsidian writes `\n`, so a note this
   server writes is LF whether or not it arrived that way. That is D8, and it
   replaced an older rule here: `line_ending` honoured whatever the note already
   used rather than normalising, which kept a CRLF note CRLF until Obsidian next
   opened it. Honouring the note was wrong because it kept the question alive in
   every join this crate generates.
2. **A Base region must not own its line ending.** See "Bugs found in this
   implementation" — a span that included the `\r` produced `\r\r\n` on restore,
   which hid the region permanently. `split_base_embeds` still trims that byte and
   `base_embed_re` still consumes it, because `write_note` parses what the AGENT
   sent and not only what the backend returned. What no longer exists is the
   machinery that matched a generated join to the note's own ending.

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

Draft paths are held to Obsidian's own portability rules, not just to "ends in
`.md`". `add_note_to_base` refuses a segment containing `\` (a separator on
Windows, where the same string walks out of the vault root), a segment starting
with `.` (`is_indexable` skips such files, so the note would be written, reported
`verified: true`, and then invisible to every query), a segment ending with `.`
or a space (Windows strips both silently), and the 28 device names Windows
reserves (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, and the
superscript variants — with or without an extension, so `NUL.tar.gz` matches).
See <https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>.

Those rules apply on **every** platform, not only where they bite, because a
guard whose safety depends on the compilation target is the defect rather than
its repair, and a `cfg`-gated rule never runs where it matters. The read side is
unaffected: an existing Linux file named `NUL.md` stays readable; only *creating*
one through `add_note_to_base` is refused.

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

### CI was red on every push and nobody saw it

`cargo test --all-targets -- --skip parity` does not exclude the parity suite.
`--skip` filters by test NAME, and only one of the four CLI-dependent tests in
`tests/parity.rs` has "parity" in its name. The other three ran, found no
Obsidian, and panicked at the guard:

```
$ OBSIDIAN_BIN=/nonexistent/obsidian cargo test --all-targets -- --skip parity
test our_json_matches_the_cli ... FAILED
test our_markdown_matches_the_cli_byte_for_byte ... FAILED
test the_cli_cannot_bind_this_and_returns_an_empty_result ... FAILED
test result: FAILED. 4 passed; 3 failed; 1 filtered out
```

The workflow comment said parity was excluded. The command did not exclude it.
This went unnoticed from `f377887` until it was looked for, which is a long time
for a red build.

**Fix.** A Cargo feature gate rather than name filtering, so the property does
not depend on anyone's naming:

```toml
[features]
parity = []

[[test]]
name = "parity"
required-features = ["parity"]
```

Plain `cargo test` then never builds the target at all, and `just parity` becomes
`cargo test --features parity --test parity` — still loud by design when the CLI
cannot answer. A CI step runs the plain command with `OBSIDIAN_BIN` pointed at a
nonexistent path and requires success, so the property cannot rot again.

`--skip parity` was also silently dropping two ordinary tests that have nothing
to do with the CLI (`render_is_the_flat_cli_parity_surface`,
`resolve_base_says_which_of_its_two_surfaces_is_the_parity_one`). Both run now.

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

> **Now unreachable.** D8 removed the machinery this entry describes.
> `line_ending` and the `ending` argument to `push_line` are gone; every join this
> crate generates is `\n`. Above the backend a note cannot be CRLF, so there is
> nothing to match a join against. Kept as history, because the reason it was
> built — "match the note's own ending, and pick the FIRST terminator so repeated
> writes converge" — is the reasoning that made normalising on read the obvious
> next step.

`push_line` appended `\n` to the joins it generates, so a deleted region restored
into a CRLF Host note came back as `…\n` between two CRLF lines — a mixed-ending
file, which is what the CRLF work in `src/note.rs` (c4af62f) existed to prevent. The
region's own bytes come from the original and carry whatever it used, but an inline
` ```base ` fence spans no terminator at all and an embed at end of file spans none
either, so the byte after a restored region is always one `push_line` chose. At that
point an embed whose own line ended CRLF appeared immune, because its span swallowed
the `\r` and `push_line`'s bare `\n` completed the pair. It was not immune; it was
wrong in the other direction, which is the next entry.

`reconcile_note` detected the note's line ending once, from the first terminator in
the original, and wrote both joins with it. First, deliberately: `write_note`
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
prose. That difference is also gone: neither implementation has a second question
to ask.

### A restored Base region came back as `\r\r\n`, and then vanished

> **The span fix stands; the doubling it caused cannot recur.** `split_base_embeds`
> still trims the byte `base_embed_re` over-consumes, because `write_note` parses
> what the AGENT sent and that text never passed through the backend — see D8. The
> `\r\r\n` on disk needed TWO CRLF artefacts meeting: a span that already ended in a
> terminator, and a `push_line` that appended the note's own ending again. Only the
> second is gone, so the doubling is unreachable. Kept as history, and kept in this
> section rather than deleted, because the lesson recorded at the end of it is the
> one that produced the loss test which replaced its regression coverage.

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
instead of merely equivalent.

The regression tests this entry cited — `expected_region`, `every_restore_shape_in_a_crlf_note_is_pure_crlf`,
`stray_cr`, and a two-deletion run through the tool surface — are **deleted**. They
asserted the SHAPE of the repair, each in one direction, and the shape they
described is gone with the machinery. What replaced them is
`no_prose_is_lost_when_a_region_is_deleted_from_either_ending` in `tests/render.rs`,
which loops over both endings and asserts the hard constraint — every prose word
survives and the Base region is a Base region again — with no counting in it at all.

The tool-surface test that went with them, two consecutive deletions through
`write_note`, is gone for a stronger reason than "the shape changed". Above the
backend a note is LF, so a restored region is restored from LF text and a second
deletion takes the same path as the first: the corruption needed a CRLF artefact
the server can no longer see. The measurement under D8 is what remains of it —
the file is not touched at all on a clean round trip, so there is nothing for a
second write to corrupt.

The lesson generalises past this bug: the assertion was written to describe the
shape we *expected* rather than the property that must hold. "No `\r` outside a
`\r\n`" catches every corruption in the class; "no lone `\n`" catches one of them,
and passed straight through this one.

## Branches

| Branch | What it is |
|---|---|
| `develop` | The Rust implementation. The shipping one. |
| `main` | Frozen at `0fbef40`. The TypeScript implementation, kept as the cross-implementation oracle. |
| `ts-implementation` | `main` plus the four data-loss fixes ported to TypeScript. |

`test/unit/*.ts` and `test/parity/*.test.ts` are cited by entries in this file and
live on those two branches only; `develop` is Rust alone.

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

**One case is no longer equivalent, deliberately.** Line endings. `develop`
normalises CRLF to LF on read (D8); `ts-implementation` preserves them. The eleven
cases above contain no CRLF note, so the measurement stands as measured — but a
CRLF note would now round-trip through one tree and not the other, and that is a
recorded Divergence rather than a bug in either.

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
