# Obsidian Bases MCP Server

## Goal

An MCP server that resolves Obsidian `.base` files to human-readable markdown, with:

- **Strict parity** with the local Obsidian CLI (`obsidian base:query`) for data (`format=json`) and presentation (`format=md`), with **exactly one documented divergence**: binding `this` to a caller-supplied embedding note.
- Correct handling of **embedded bases** whose filters reference `this.file` — the case the Obsidian CLI gets wrong (it silently returns `[]`).
- A **read/write vault backend** abstraction with local filesystem and WebDAV implementations that produce identical results.
- Safe write semantics: patching a note **never** changes or removes an embedded base by default.

## Settled decisions

| Decision | Choice |
|---|---|
| Runtime | TypeScript on Bun |
| Evaluator | Own implementation: lexer → Pratt parser → evaluator, written against the official spec |
| Parity contract | Strict vs `format=json` and `format=md`; one documented divergence |
| Language scope | Full documented spec, staged; **hard errors** on anything unimplemented |
| Note model | Ordered segments; a `.base` embed is an opaque `BaseEmbed` segment |
| Read projection | Embed replaced by a ```` ```base-rendered ```` fence with provenance |
| Write default | Never change or remove base regions; other changes apply |
| Refused base edit | Partial apply + structured error explaining how to create matching notes |
| Markdown rendering | `table`/`cards`/`kanban` → md table; `list`/`map` → md list |
| Tool surface | 6 tools: `list_bases`, `resolve_base`, `get_note`, `write_note`, `add_note_to_base`, `backlinks` |
| `add_note_to_base` | Draft → agent edits → verify → commit, with base YAML returned on failure |
| WebDAV | First-class backend, read+write from the start |
| Divergence policy | **Spec wins**; log every divergence, never block |
| CLI parity tier | Always-on (CLI is running locally) |
| Testing vault | Realistic project vault + feature-matrix fixtures, inside the repo |

### Defaults set without a decision (change freely)

- One vault per server process, configured by env: `BASES_MCP_VAULT` (fs path) **or** `BASES_MCP_WEBDAV_URL` + `BASES_MCP_WEBDAV_USER` + `BASES_MCP_WEBDAV_PASSWORD`.
- Drafts for `add_note_to_base` are held in an in-memory map keyed by `draft_id`, with a TTL. Not persisted across restarts.
- `file.tasks` is implemented as a documented extension (see Risks).

## Reference material to persist

Create under `docs/`:

- `sources.md` — the four URLs the user supplied plus everything the research agents found, each with a one-line note on what it is and whether it is canonical.
- `bases-spec.md` — the distilled official spec: file format, filter grammar, function tables, property namespaces, `this` resolution, error strings. The canonical source is raw `obsidianmd/obsidian-help@master` (`en/Bases/Bases syntax.md`, `en/Bases/Functions.md`), because `obsidian.md/help/bases` is a client-rendered shell that returns only `<title>` to a plain fetch.
- `divergences.md` — the divergence registry. Every row: construct, our behaviour, Obsidian's behaviour, evidence URL, and a `verify-in-obsidian:` flag for items the user checks by hand.
- `coverage.md` — spec construct → supported / not yet / planned.

Vendored skills go in `.kilo/skills/` (project-scoped), from `kepano/obsidian-skills` (MIT, keep the LICENSE). Vendored copies carry **four known defects** to fix at copy time:

1. `.days`/Duration guidance contradicts the official spec (issue #137) → use `((date(due) - today()) / 86400000).round(0)`.
2. "Links" is not a property type and **Tags** is missing (issue #139).
3. View-level `sort` is undocumented in the skill (issue #109).
4. The `` `!`, `` substring in the special-characters list crashes Codex/Kilo-style skill loaders (PR #116) — move the backtick entry to the end.

Only `obsidian-bases`, `obsidian-markdown`, and `obsidian-cli` are needed (4 files, ~22 KB, zero dependencies).

## Domain glossary (`CONTEXT.md`)

Terms are domain-specific; keep the file a glossary only.

- **Base** — a `.base` file: YAML defining views, filters, formulas, properties, and summaries over the whole vault.
- **View** — one named query-and-layout pair inside a base (`type: table | cards | list | kanban | map`).
- **Host note** — the note a base is embedded in; the binding target for `this`.
- **Base region** — the byte span in a host note occupied by an embedded base. Opaque to writes.
- **Projection** — the agent-facing rendering of a note with base regions replaced by rendered fences. **Never written back to disk.**
- **Property ID** — a `note.`/`file.`/`formula.`-prefixed column identifier. Bare keys normalize to `note.`.
- **Divergence** — a deliberate, documented difference from Obsidian's behaviour.

## Architecture

```
src/
├─ vault/        VaultSource interface; FsVaultSource; WebDavVaultSource; snapshot + invalidation
├─ note/         markdown segmentation; frontmatter; wikilinks; tasks; ```base fences
├─ expr/         lexer.ts, parser.ts (Pratt), evaluator.ts, stdlib/
├─ bases/        .base YAML parse, query pipeline (formulas → filters → sort → group → limit → summaries)
├─ render/       markdown renderers per view type
└─ mcp/          tool definitions, server wiring, draft store
```

`VaultSource` is the only I/O boundary:

```ts
interface VaultSource {
  list(): Promise<string[]>                        // vault-relative posix paths
  readText(path: string): Promise<string>
  writeText(path: string, data: string): Promise<void>
  stat(path: string): Promise<{ size: number; mtime: Date }>
  hash(path: string): Promise<string>              // our own, for read-verify-write
}
```

Both implementations return identical resolution results for the same content; that equivalence is a tested invariant.

### Evaluation pipeline

1. Parse `.base` YAML. Unknown view keys are **preserved verbatim and never interpreted** — the view level is a documented open namespace written to by plugins.
2. Select view: `#View` name match, else `views[0]`.
3. Evaluate formulas once (not per note), with dependency ordering and cycle detection.
4. Apply global `filters` AND view `filters`.
5. Sort, group, limit, summarize.

Hard errors — never `null` — for: unknown function, unknown property namespace, unimplemented grammar, and `filters` with sibling `and`/`or`/`not`.

### `this` binding

`this` resolves to the **host note**. When a base references `this` and no host is supplied, **raise a hard error — do not return an empty array.** Obsidian returns `[]` and that behaviour is the documented divergence.

## MCP tool surface

| Tool | Behaviour |
|---|---|
| `list_bases` | All `.base` files with their view names and types |
| `resolve_base` | `base`, `view?`, `context?` (host note path), `format?` (`markdown` \| `json`), `includeAllFormulas?` |
| `get_note` | `path`, `raw?` (default `false` → bases inlined). Returns the projection. |
| `write_note` | `path`, `content` (the agent's edited note). Applies non-base changes, restores base regions byte-for-byte, returns applied/refused report. |
| `add_note_to_base` | Draft → verify → commit handshake (below). |
| `backlinks` | `path` — inbound links, using the same resolver as Bases |

### Projection fence

````
```base-rendered path="Tickets.base" view="All" context="Projects/SomeProject.md"
| Note | Status | Priority |
| --- | --- | --- |
| [[Fix login]] | active | 2 – normal |
```
````

Any language other than `base` is inert in Obsidian, so this is safe to display. The `context=` field is what the CLI cannot express.

### `write_note` reconciliation

1. Parse the stored note → segments; record base regions as byte ranges.
2. Parse the agent's content → locate corresponding regions (fence info string + path, falling back to positional match).
3. For each region: identical → keep. Different → **restore ours**, record a refusal.
4. Apply all remaining changes.
5. Return: written path, applied change summary, and a structured `error` per refusal explaining that adding rows to a rendered base is not supported, how to create a note that matches instead, and the base's YAML.

### `add_note_to_base` handshake

1. `add_note_to_base(base, view?, path, context?)` → **best-effort filter inversion** produces a frontmatter draft (e.g. `file.hasTag("ticket")` → `tags: [ticket]`; `project.contains(link(this.file.name))` → `project: ["[[<context basename>]]"]`; plus required columns from `order`). Un-invertible expressions become clearly-marked placeholders.
2. Return `draft_id` + the draft note.
3. Agent edits and calls `add_note_to_base` again with `draft_id` and its edited content.
4. Verify by running the base's real filter against the draft.
5. Success → write and return the note. Failure → error plus the base's YAML so the agent can correct and resend.

## Phases

### Phase 0 — Scaffold, research artifacts, oracle setup

1. `bun init`, `tsconfig.json` strict, `bun test` wired, ESLint.
2. Verify `obsidian version` ≥ 1.12.7 and that `base:query` runs. **Record the actual version in `docs/divergences.md`.**
3. Probe `file.tasks` against the live CLI — does `file.tasks.length` resolve in 1.12.x? Record the answer either way.
4. **User action: register `<repo>/test/vault` in Obsidian** (Open folder as vault). Obsidian writes `.obsidian/` there → gitignore it.
4b. **Build the testing vault.** Two layers:
   -  — a realistic project vault. Must include your `Tickets.base` / `SomeProject.md` case verbatim, plus a second project note embedding the *same* base to prove the scoping differs by host, and an inline ```` ```base ```` block variant.
   -  — one small vault per spec construct, plus an adversarial set seeded from these real corpora, which are the highest-value hostile material found:
     - `callumalpass/tasknotes` `src/templates/defaultBasesFiles.ts` — `list().filter().isEmpty()`, `this.file.asLink()` inside a lambda, regex `replace(/…/, "$1")`, `reduce(acc + value, 0)`, bracket access `note["pomodoros"]`, 4-deep nested `and`/`or`, `file.tasks` in `order:`, and `sort:` using the legacy `column:` key.
     - `rafafields/Obsidian-Base-Hub` — 17-view base with multi-key `sort`, `columnSize`, `cardSize`, and link equality `status == link("05 - Defined 🔵")` (emoji filenames).
     - `jackyzha0/quartz` `docs/Base.base` — a `not:` with **six** sibling entries (the NAND trap), plugin view types `board`/`gallery`, block-scalar `|` formulas.
     - `elecdot/vault` — `(now() - file.mtime).days.round(0)`, a bare `- formula.needs_follow_up` used directly as a truthy predicate, `file.links.filter(!value.asFile().isTruthy()).length`, views with **no `order:` at all**.
     - A CJK-`displayName` case, and a note containing two embeds of the same base with different `#View` selectors.
   - Also encode the link-resolution rule from that corpus's live CLI log: **an ambiguous `[[Readme]]` resolves to the shortest path**, not a same-folder sibling.
5. Write `docs/sources.md`, `docs/bases-spec.md`, `docs/divergences.md` (seeded), `CONTEXT.md`, and ADRs.
6. Copy + fix the three vendored skills into `.kilo/skills/`.

### Phase 1 — Vault and note model (local fs only)

7. `VaultSource` interface + `FsVaultSource` + snapshot with explicit invalidation.
8. Note segmentation: frontmatter (YAML), wikilinks, `![[embeds]]` (including `![[X.base#View]]`), ```` ```base ```` fences, task lines. Segments must carry byte offsets and reproduce the input byte-for-byte when concatenated.
9. `file.*` model: `name` (**with extension**), `basename`, `path`, `folder`, `ext`, `size`, `ctime`, `mtime`, `tags` (frontmatter + inline), `links`, `embeds`, `backlinks`, `properties`, `file`.
10. Link resolution matching Obsidian semantics: basename match, **bare `[[Alias]]` does not resolve** (aliases feed the suggester only), ASCII-folded/diacritic-insensitive comparison, path-substring then least-residue ranking.
11. Support the **union** of both official `file.*` tables — the two official help pages disagree.

### Phase 2 — Expression engine

12. Lexer → Pratt parser → evaluator. Member access on bare numeric literals (`(1).isTruthy()`) and object literals must work — Obsidian's parser rejects both and we deliberately follow the docs (obsidian-help issue #1095).
13. Stdlib: globals (`if`, `date`, `duration`, `now`, `today`, `number`, `min`, `max`, `list`, `link`, `file`, `image`, `icon`, `html`, `escapeHTML`, `random`), plus methods for string/number/list/date/duration/link/file/object/regex. Include regex literals, `regexp.matches`, `$1` capture groups in `replace`.
14. Semantics: `not:` = "none of these are true" (NAND, not logical negation); `contains()` dispatches on three receiver types and is type-strict; empty list is falsy; `date.isEmpty()` is always false; `filters` allows exactly one of `and`/`or`/`not`; `M`=month, `m`=minute in duration strings; duration must be on the left in duration×scalar.
15. **Duration semantics — support both, log both.** Obsidian's docs say date subtraction yields milliseconds; the runtime returns a `Duration` and `number(duration)` throws (obsidian-help issue #1095). Real vaults depend on **both** — tasknotes uses `((number(date(due)) - number(today())) / 86400000).floor()`, while a published vault uses `(now() - file.mtime).days.round(0)`. So: implement `Duration` with `.days/.hours/.minutes/.seconds/.milliseconds` fields, *and* make `number()` on a duration yield milliseconds. Both idioms then work; record the conflict in `divergences.md`.
16. Match Obsidian's exact error strings where known: `"filters" may only have one of an "and", "or", or "not" keys.`, `Type error in "contains", parameter "value". Expected String not, given File.`
17. Implement `file.tasks` (task parsing on top of the existing body scan), matching the `tasks` CLI's shape. Mark as an extension in `divergences.md` unless Phase 0 proved Obsidian 1.12.x ships it natively.

### Phase 3 — Base query pipeline + parity

18. `.base` YAML parsing. Preserve unknown view keys verbatim. Normalize bare property IDs to `note.`. Accept both `sort[].property` and legacy `sort[].column`.
19. Query pipeline with formula dependency ordering and cycle detection.
20. **Parity harness** (`test/parity/`), with all of these landmines handled:
    - `vault=` must be the **first** parameter.
    - `base:query file="X"` needs the `.base` extension — prefer `path=` entirely.
    - **Never trust the exit code.** Unknown flags are ignored and still exit 0. Validate output shape instead.
    - **stdout race**: heavy formula queries lose output on pipes — observed at 10 MB returning *completely empty*, exit code 0. Detect truncation and retry; never assert on a truncated capture.
    - JSON keys are **display labels, not property IDs**. `file.path` is emitted as `"file path"` and `Note.md` as `"note"` — display labels, sometimes with an inserted space. Map back through `properties.displayName` **plus** the `file.X` → `file X` rule before comparing.
    - **All values arrive stringified.** Compare after normalizing both sides to strings, or type-normalise explicitly.
    - A **zero-row** view yields a degenerate shape (`columns: ["path"]` only). Handle it rather than treating it as a parse failure.
    - `base:views` ignores `file=`/`path=` — do not use it for per-base view listing.
    - Bases using `this` return `[]` — assert that as the *known divergence*, not a failure.
21. Assert row **order** parity while logging `groupBy`-ordering mismatches as a known CLI bug rather than failing.

### Phase 4 — Rendering and read tools

22. Markdown renderers per the settled mapping. `groupBy` and `summaries` preserved as header/footer rows. Unknown view type → hard error.
23. Build the projection: segments → note text with ```` ```base-rendered ```` fences carrying `path`/`view`/`context`.
24. Wire `list_bases`, `resolve_base`, `get_note`, `backlinks`.
25. Three-state result health (`ok` / `partial-with-errors` / `failed`) — Obsidian reports an error *and* correct rows simultaneously, which we refuse to imitate.

### Phase 5 — Write path

26. `write_note` reconciliation with base-region protection and partial-apply-plus-structured-error.
27. `add_note_to_base` draft store, filter inversion, verify, commit.

### Phase 6 — WebDAV backend (after local vault works end-to-end)

28. WebDAV client on Bun's native `fetch` + `fast-xml-parser` — or the `webdav` npm client if a Phase 6 smoke test proves its `node-fetch` resolution path works. Recurse with **repeated `Depth: 1`; never send `Depth: infinity`** (no mainstream server supports it).
29. `compose.yaml`: `sigoden/dufs:0.46.0` with a named volume, Basic auth, pinned tag, loopback-only port. Optional Tier 2 service: `rclone serve webdav` in front of MinIO, with `--vfs-cache-mode=writes` (not the `off` default) and `--dir-cache-time=1s` (not the 5-minute default).
30. Seed script that loads  into the server.
31. `WebDavVaultSource` with read+write. **Writes use our own content hash for read-verify-write, not server ETags** — ETags are unproven on dufs and absent from its test suite.

### Phase 7 — Docs and polish

32. Fill in `docs/coverage.md`. Finalize `docs/divergences.md`, marking items needing your manual verification in real Obsidian.
33. README covering the 6 tools, the fence contract, and the write-safety guarantees.

## Validation

| Suite | What it proves | Runs when |
|---|---|---|
| `test/unit/` | Lexer, parser, evaluator, segments, links, renderers | Always |
| `test/conformance/` | Every spec construct has a golden, incl. the docs-vs-runtime divergences | Always |
| `test/parity/` | Byte-comparable output vs `obsidian base:query format=json` and `format=md`, over the whole testing vault | When the CLI is runnable |
| `test/webdav/` | Same vault over WebDAV yields identical resolution output as fs | When the compose stack is up |

**Invariants asserted in every phase:**

- Resolving a note containing a base yields identical output over fs and WebDAV.
- `write_note` never alters a base region's bytes, and never deletes an embed.
- Every unimplemented construct produces a hard error, never a silent `null`.
- A `.base` file is never written by any note operation.

## Risks

- **`file.tasks` may not exist in Obsidian.** Documented as absent; TaskNotes docs suggest it landed undocumented in 1.12.x. Phase 0 probes it. Either way it is implemented; if native, we match Obsidian, and if not, it is a logged extension.
- **Obsidian version drift.** The CLI's JSON shape, error strings, and quirks change between releases. Parity tests will fail loudly on a version bump — that is the intended signal, not a flake.
- **`\x1f` composite keys.** Research found the kanban plugin using `||`, not `\x1f` — but the user's own `Tickets.base` contains `\x1f` keys, which is first-hand evidence they occur. Handling: preserve verbatim, never interpret, never choke.
- **Filter inversion is best-effort.** Arbitrary filter expressions are not reliably invertible. The draft may contain placeholders; the verify step is the guarantee, not the inversion.
- **`file.backlinks` and `file.properties` are stale in Obsidian by design** ("does not automatically refresh"). We compute deterministically from a snapshot and document the freshness model.

## Open questions

None blocking. Deferred by agreement:

- Client-side editing of `.base` files and of inlined base regions, once we can verify validity — future work.
- Faithful markdown for `cards`/`kanban` beyond a flattened table.