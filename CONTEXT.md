# Bases MCP

An MCP server that resolves Obsidian `.base` files to human-readable markdown, with strict parity against the local Obsidian CLI and correct handling of embedded bases that scope themselves to the note they live in.

## Language

**Base**:
A `.base` file: YAML defining one or more Views plus the Filters, Formulas, Properties and Summaries they draw on. It queries the whole vault — there is no `from` clause.
_Avoid_: query, database, collection

**View**:
One named query-and-layout pair inside a Base. The layout is chosen by its `type` (table, cards, list, kanban, map) and its remaining keys are an open namespace that view type interprets.
_Avoid_: report, layout, page

**Host note**:
The note a Base is embedded in. It is the binding target for `this`, and therefore the thing that decides which rows an embedded Base shows.
_Avoid_: embedding note, parent note, context note

**Base region**:
The byte span in a Host note occupied by one embedded Base. Opaque to every write operation.
_Avoid_: embed, block, placeholder

**Projection**:
The agent-facing rendering of a note with each Base region replaced by a rendered fence. Never written back to disk — Obsidian would reject it, since a `base` fence must contain live YAML.
_Avoid_: rendering, export, serialisation

**Property ID**:
A column identifier, always prefixed with `note.`, `file.` or `formula.`. A bare identifier normalizes to `note.`.
_Avoid_: field, key, column name

**Link value**:
A property value that points at another note, compared by resolved target rather than by its text. Distinct from the plain string `"[[Some Note]]"` when the two are stored differently.
_Avoid_: wikilink, reference

**Divergence**:
A deliberate, documented difference from Obsidian's behaviour. Every one is recorded with evidence; none is a bug.
_Avoid_: exception, deviation, gap

**Divergence registry**:
`docs/divergences.md` — the single place a Divergence may be recorded.
_Avoid_: changelog, notes

**Fixture vault**:
A self-contained mini-vault under `test/fixtures/`, isolating one spec construct. Each has its own `.base` and its own notes, and is resolved by pointing the vault backend at its directory. Never registered in Obsidian — fixtures are exercised by the unit and conformance suites, not by CLI parity.
_Avoid_: test case, sample

**Testing vault**:
The vault at `test/vault/`, holding a realistic project layout. Registered inside Obsidian so the CLI can query it. Contains `Tickets.base` (which scopes itself with `this`, exercising the divergence) and `AllNotes.base` (which does not, so the CLI is a genuine parity oracle).
_Avoid_: demo vault, sample vault