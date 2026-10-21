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
| **Evidence** | The five sources under "Obsidian writes LF on every platform", including the Obsidian staff reply that there is no way to change this. Our side is measured by `the_two_spellings_of_a_note_get_the_same_base_hash` and `a_base_hash_from_a_crlf_note_round_trips_through_an_obsidian_resave`, and the no-op round trip is pinned by `a_crlf_edit_that_changes_nothing_does_not_touch_the_file`, all in `tests/service.rs`. |
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
  reached through a path nobody was watching. **Pinned by a test.**
  `a_crlf_edit_that_changes_nothing_does_not_touch_the_file` in `tests/service.rs`
  takes a CRLF Host note through the same full Projection round trip and compares
  the file's BYTES before and after rather than its length: the LF pin
  (`an_edit_that_changes_nothing_does_not_touch_the_file`) compares `.len()`, which
  would not see a same-size rewrite that changed the endings. Deleting the
  normaliser makes the new test fail — the reconciled text no longer equals the
  note that was read, so the no-op writes.

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

## D9 — The HTTP transport is network-accessible, unauthenticated, and trusts the network

| | |
|---|---|
| **Construct** | The Streamable HTTP transport: `--transport http` / `MCP_TRANSPORT=http`, endpoint `http://HOST:PORT/mcp` |
| **Ours** | The listener is bound to whatever `MCP_HTTP_HOST` names — `127.0.0.1` by default, any interface at `0.0.0.0` — and answers any client that can reach the socket. There is **no authentication**, **no authorization**, and **no rate limiting**. `Origin` validation is disabled (`allowed_origins` is empty, which is rmcp's default), so a browser page can issue a cross-origin request and it is served. |
| **Obsidian** | Does not apply. Obsidian has no HTTP transport and no listener; it is a desktop application. There is no upstream behaviour to diverge from — this entry records a property of a surface that did not exist before this branch, so that the trust model behind it is written down rather than assumed. |
| **Evidence** | Measured on this branch at `4f2c651` — the commit that derives `allowed_hosts` from the bind address — against the built binary: the listener starts on the bound address; a request from a second process on the same host completes a full `initialize` → `tools/list` → `tools/call` handshake with no credential of any kind; an `Origin` header is accepted rather than checked; and `MCP_HTTP_HOST=0.0.0.0` makes every `Host` header acceptable (see below), so the listener serves any address the host has. |
| **verify-in-obsidian** | no — the surface is ours, not Obsidian's |

The **trust model is that the network is safe.** The server is expected to run
in a private network with no public exposure. That assumption is not enforced by
the code and nothing in this repository verifies it: it is an operator decision,
not a property of the build. A deployment that binds `0.0.0.0` on an untrusted
link has an unauthenticated, network-reachable server that can read and write
the vault it is scoped to, and no record of who called what.

This is a **recorded, accepted gap**, not a claim that the work below is done.
The hardening list — authentication, authorization, rate limiting, TLS, input
validation at the resolver boundary, and audit logging — is kept outside the
branch, at the repository root, because working docs are not branch content.
It should be re-opened before this server is ever exposed beyond the network
it is currently trusted on.

### What is enforced anyway

Three things are not left to the operator, and are pinned by tests in
`tests/http_transport.rs`:

- **A `Host` that names the bound address is served; a neighbour is refused.**
  rmcp validates `Host` against `allowed_hosts` and answers a request it does
  not recognise with `403 Forbidden: Host header is not allowed`. Before this
  branch `allowed_hosts` was rmcp's hardcoded loopback list
  (`localhost`, `127.0.0.1`, `::1`), so `MCP_HTTP_HOST=0.0.0.0` bound every
  interface while every non-loopback `Host` was refused: a listener that was
  reachable on the network and served nothing, with no diagnostic. The list is
  now derived from the address the operator bound — a specific address is served
  on that address and on `localhost`; `0.0.0.0` / `::` allow **every** host,
  because the address a request arrived on is not knowable at bind time.
  `0.0.0.0` therefore widens what is accepted by design, says so on stderr at
  startup, and is the operator's explicit choice.
- **A bad bind address or port is a refusal, not a fallback.** `MCP_HTTP_HOST`
  and `MCP_HTTP_PORT` name the variable and the value they rejected, and a
  set-but-blank port is an error rather than an absence. Before this branch a
  typo'd host silently fell back to `127.0.0.1` and a blank port to `8765`.
- **The stdio path is not a separate server.** `tests/stdio_transport.rs` spawns
  the real binary and drives `initialize` → `tools/list` → `tools/call` over
  stdin/stdout, so a change that broke the default transport while every HTTP
  test stayed green would fail there rather than ship.

The remaining gap is the network, not the transport: the endpoints above are
about *which* client reaches the server, and nothing here decides *whether* the
server should be reachable at all.

---

## D10 — A method call on a null receiver answers null

| | |
|---|---|
| **Construct** | A method call whose receiver is a property the note does not have, e.g. `type.contains("task")` against a note with no `type` |
| **Ours** | The call answers **null**. A null filter expression is falsy, so the row is **dropped** and the query still answers. A non-null receiver with an unknown method still raises `Type error: "x" is not a method on <type>`. |
| **Obsidian** | The same: the call answers null, the null filter is falsy, and the row is dropped. No error is reported. |
| **Evidence** | Read out of Obsidian's own Bases evaluator, which ships in `resources/obsidian.asar` (installer 1.12.4 — the app `docs/divergences.md` records as 1.13.7). Its call site resolves the receiver, looks the method up on the receiver's type chain, and then: `if(s=r.getValue(e)||gW.value, !(t=KW.findForValue(s,i)) && s===gW.value) return gW.value`. `gW.value` is Obsidian's null singleton, so a null receiver with an unknown method answers the null value instead of raising. Obsidian's filter predicate is `this.getValue(e).isTruthy()`, and its null value answers `isTruthy()` false. Pinned by `tests/null_property.rs` (3 tests) and `tests/expr.rs` (`a_null_receiver_answers_null_whatever_the_method_is_called`, `the_universal_methods_answer_on_a_null_receiver`, `a_non_null_receiver_with_an_unknown_method_still_errors`). |
| **verify-in-obsidian** | no — the rule is in Obsidian's shipped evaluator, not in the CLI's rendering of it |

**Why this is a Divergence and not a bug.** Before this change the Rust
implementation raised `Type error: "contains" is not a method on null`, which
failed the WHOLE query rather than dropping one row. That was a divergence
from Obsidian's behaviour, so it is recorded here rather than in the Bugs
section below.

**What the four universal methods do.** Obsidian registers exactly four
methods that a null receiver resolves to — `isEmpty` on `Null` itself, and
`toString`, `isTruthy` and `isType` on `Any`, which `Null` extends — and its
lookup walks that prototype chain, so they are found before any fallback can
answer for them. Ours registers the same four, and the null-receiver
fallback sits behind them, so a null receiver reaches the same four methods
and nothing else. Obsidian's null value also says `toString()` is the string
`"null"` (not the empty string), which is why a null cell and a cell holding
the text "null" are different things on the JSON surface.

**The TypeScript tree has not been repaired.** `ts-implementation` still
raises `Type error: "contains" is not a method on null` for the same
construct, because its `getMethod` has no null-receiver fallback. Measured
against `ts-implementation` at `9a86bd5`:

```
missingprop.contains("x")  -> ERROR Type error: "contains" is not a method on null
missingprop.bogus()        -> ERROR Type error: "bogus" is not a method on null
missingprop.toString()     -> ""          (Obsidian: "null")
status.bogus()             -> ERROR Type error: "bogus" is not a method on String
```

So the two implementations no longer agree here, and this is the second
recorded place where they differ on purpose — the first was line endings
(D8). The TypeScript side is not touched by this change; it is a separate
repair.

**What is NOT settled by this entry.** Obsidian's rule for a null
*argument* — as opposed to a null receiver — is a different question and is
not recorded here. Obsidian's own evaluator distinguishes three cases (a
null argument makes the whole call answer null; a null argument in an
*optional* position is treated as absent; an argument that is simply
**missing** from the call site is `Cannot call function "x", not enough
arguments.`), and the Rust implementation does not yet model that
distinction. It is tracked separately.


## D11 — The frontmatter coercion chain is Link → URL → Date

| | |
|---|---|
| **Construct** | A frontmatter value written as a quoted date, a URL, or a wikilink |
| **Ours** | A frontmatter STRING is coerced before it is typed, in the order **Link → URL → Date**: `[[Some Note]]` is a Link, `https://example.com` is a URL, `2026-01-05` is a Date. A markdown link (`[Business](Projects/Business)`) is **not** coerced and stays a String. The `tags` key never reaches the chain: a tag is typed as a `Tag` when the frontmatter is read, which is why a tag spelled `[[x]]` stays a tag. |
| **Obsidian** | The same, and this entry used to be a **divergence in our favour**: we coerced markdown links (Obsidian does not) and we coerced nothing else (Obsidian coerces dates and URLs). |
| **Evidence** | `base:query` against Obsidian 1.14.4 (installer 1.12.4), with `isType` formulas asking each value what it is. Recorded in the table below. |
| **verify-in-obsidian** | no — the CLI evidence is decisive, and it was taken against a live app |

**What the CLI answered.** Every row was measured with
`obsidian base:query path=… format=json` against a probe vault, asking each
value `isType("…")`:

| frontmatter | typed as | note |
|---|---|---|
| `"2026-01-05"` | **Date** | local midnight; `isType("string")` is **false** |
| `2026-01-05` (unquoted) | **Date** | YAML parses it as a timestamp |
| `"2026-01-05T17:30:00"` | **Date** | `.hour` → 17, local time |
| `"2026-01-05 17:30"` | **Date** | the space separator is accepted |
| `"2026-01-05T17:30:00+02:00"` | **Date** | rendered in LOCAL time → 16:30 here |
| `"2026-01-05T17:30:00Z"` | **Date** | → 18:30 local |
| `"2026-1-5"` | **String** | the month and the day must be two digits |
| `"2026-13-45"` | **Date**, invalid | `.year` → NaN; it is not null |
| `" 2026-01-05 "` | **String** | **nothing is trimmed** |
| `"[[Some Note]]"` | **Link** | |
| `"[[Some Note\|Alias]]"` | **Link** | split at the FIRST `\|` — the same split `extract_links` makes for a body link |
| `"[[Some Note#Section]]"` | **Link** | the `#heading` stays IN the target |
| `" [[Some Note]] "` | **String** | a leading space defeats the match |
| `"https://example.com"` | **URL** | also `ftp:`, `mailto:`, `foo:bar`, `a:b` |
| `"HTTPS://EXAMPLE.COM"` | **URL** | the scheme is case-insensitive |
| `"example.com"` | **String** | no scheme |
| `"//example.com"` | **String** | |
| `"https://"` | **String** | an empty host is not a URL |
| `"https://example.com/a b"` | **String** | **a space disqualifies it** |
| `"file:///etc/passwd"` | **URL** | a `file:` URL is a URL |
| `"C:/path"` | **URL** | a Windows path parses as a URL |
| `"[Business](Projects/Business)"` | **String** | **a markdown link is never coerced** |
| `"[Home](https://example.com)"` | **String** | |
| `tags: ["[[Some Note]]"]` | **Tag** | `tags` is typed before the chain; a tag is string-like, never a Link |

The URL test is exactly Obsidian's: **the string must contain no space, and
`new URL(text)` must not throw.** The WHATWG parser decides the rest, so
`https://` fails on an empty host while `mailto:a@b.com`, `foo:bar` and
`file:///etc/passwd` all pass. Rust's `url` crate implements the same parser,
and it agrees with the CLI on every row in the table above.

`tags` never reaches the chain: the YAML reader types it first. A `tags`
property that is a string, or a list of strings, becomes a **tag list** — a
`Tag`, which is string-like and carries a forced `#` — at any depth and
case-insensitively (the rule is on the key name). A mixed list is left exactly
as written, so its strings stay Strings. `aliases: ["[[Some Note]]"]` is not
`tags`, so it IS coerced, to a Link.

**One deliberate difference.** Obsidian recognises an out-of-range date field
(`"2026-13-45"`) and produces an INVALID Date whose `.year` is `NaN`. A
`BasesDate` holds a real instant and cannot carry `NaN`, so we leave such a
string a **String**. It is the only row in the table we do not match, and it is
narrower than it sounds: every date Obsidian can represent, we coerce.

**`isType` walks a prototype chain.** It does not compare one type name
against one type name, so a value answers for every type it inherits from:

| value | answers |
|---|---|
| URL | `url`, `string`, `any` |
| Link | `link`, `string`, `any` |
| Date | `date`, `any` — **not** `string` |
| Number | `number`, `any` — **not** `string` |
| everything else | its own type, and `any` |

**A URL and a Link inherit the String method set**, and what they inherit
operates on their own text: `link.length` is the length of the TARGET (9 for
`[[Some Note]]`), not of the `[[…]]` spelling, and `link.contains("Some")`
matches the target. A URL has **no** `asFile`, and a Date has **no** `asFile`
either. Obsidian's Link class has no `target`, `display` or `path` member at
all — asking for one is a hard error there, and ours keeps them, which is
recorded below rather than here.

**Dates are LOCAL, and a date-only value is local midnight.** Measured on a
host at UTC+01:00, `number("2026-01-05")` is `1767567600000`, which is
2026-01-05T00:00:00+01:00 — not UTC. The `date()` GLOBAL is read the same way,
so `date("2026-01-05")` and a frontmatter `"2026-01-05"` are the same instant.

**Equality follows from the types.** A Link equals its target, its `[[…]]`
spelling and its `[[…|alias]]` spelling; a URL equals its text; a date-only
value equals a datetime on the same day (`d == dt` is true).

**Why the chain is in this order.** A `[[…]]` is checked first because it is
the only form with brackets, and a URL before a date because `2026-01-05` is
not a URL while `https://…` is not a date. The two are disjoint, so the order
between them is unobservable — but the Link check must come first, because a
URL may contain a `[[` in its path.

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
| **The string methods do NOT share one case rule** | `contains` is **case-insensitive**; `==`, `startsWith` and `endsWith` are **case-sensitive** | The obvious expectation is that a family of search methods behaves uniformly. It does not, and it is Obsidian's asymmetry. See the table below. |
| **`contains("")` is false, but `containsAll("")` and `containsAny("")` are true** | The empty-needle guard is on `contains` alone | Every other string contains the empty string, and the variadic forms do behave that way. See the table below. |
| **A link's `contains` compares the RAW TARGET, not the `[[...]]` rendering** | `linkish.contains("[")` is **false** | The brackets look like part of the value. They are not — they are the rendering. See the table below. |

### The case rules of the string methods

Measured against the live Obsidian CLI on **1.14.4**, over a throwaway vault
holding one note with `title: Hello World` and `mixed: Hello World`. Every
cell below is a CLI answer, and every one of them is pinned by
`contains_folds_case_and_the_other_three_do_not` in `tests/base.rs` over the
`string-case` fixture, so the four cannot drift apart again.

| Construct | Obsidian | Why it looks wrong |
|---|---|---|
| `"Hello World".contains("hello")` | **true** — case-insensitive | A case-insensitive search in a case-sensitive language. |
| `"Hello World".contains("HELLO")` | **true** | Same rule, other direction. |
| `"Hello World".contains("")` | **false** | Every other string contains the empty string. |
| `"Hello World".containsAll("")` | **true** | The variadic forms have no empty-needle guard. |
| `"Hello World".containsAny("")` | **true** | Same. |
| `"Hello World".startsWith("hello")` | **false** — case-sensitive | `contains` folds and this does not. |
| `"Hello World".endsWith("world")` | **false** | Same. |
| `"Hello World" == "hello world"` | **false** | Equality is case-sensitive, as everywhere. |
| `"Hello World".startsWith("")` | **true** | The empty-needle guard belongs to `contains` alone. |

**Why this is not a Divergence.** We match Obsidian on all of it. It is
recorded here because the ASYMMETRY is the kind of thing that gets "fixed" by
someone reading the method list and assuming uniformity: folding
`startsWith`/`endsWith` to match `contains` would make the family uniform and
would **introduce** a divergence from Obsidian. The entry is a warning
against a well-meaning change, pinned by a test so the warning survives.

**The empty-needle guard.** Obsidian's own implementation is
`"" !== needle && haystack.toLowerCase().includes(needle.toLowerCase())` —
the guard is on the NEEDLE, and the variadic forms
(`containsAll`, `containsAny`) have no such guard, so they fold the case and
call `includes` per needle, and `includes("")` is true. A single
`str_contains` helper for all three would have been wrong on exactly this
point, which is why the guard lives in one place and the case folding in
another.

**A link's `contains` compares the raw target.** Obsidian's `LinkValue` is a
SUBCLASS of `String` whose data is the target the author wrote, so
`linkish.contains("[")` is false for `linkish: "[[Hello World]]"` — the
brackets are the rendering, not the data. The rendering is reachable through
`toString()`, which answers `[[Hello World]]`. We compare the raw target for
the same reason.

**Not the same as the list receiver.** A note property whose inferred type is
`multitext` is promoted to a LIST for these five methods, and a list answers
`contains` by ELEMENT equality — `words.contains("hello")` is false for
`words: [Hello, World]`, and `words.startsWith(...)` is refused outright
("Cannot find function startsWith on type List"). That promotion is a
type-coercion question rather than a case question, so it is not settled
here; it belongs with the frontmatter-coercion work.

**The TypeScript tree has the same divergence.** `main` and `ts-implementation`
implement `contains` as `str(target).includes(needle)` — case-SENSITIVE, and
with no empty-needle guard — so they answer this the way we used to. Those
branches are the cross-implementation ORACLE rather than the shipping tree
(`develop` is the shipping one), and `main` is frozen at `0fbef40`, so this
entry records the divergence rather than repairing it there. The Rust side is
the one the project ships, so the Rust side is the one that was repaired.

| `limit: 0` | **No limit at all** — the view keeps every row | The one value that looks like "keep nothing" and means "keep everything". Obsidian's view parser only keeps a limit that is a number AND positive. |
| `order: []` | Falls back to the single `file.name` column | An empty array is falsy in JavaScript, so Obsidian's own accessor (`this.order \|\| ["file.name"]`) never sees it as "no columns". |
| Nulls in a sort | Sort **last**, in **both** directions | The direction is applied to the comparison of two VALUES, not to the null check, so a row with no value stays at the bottom whichever way the sort runs. A descending sort that simply reversed the ascending order would put it first. |
| The columns of a view | Exactly the ones the view **orders** | A property the Base *declares*, or one the view *sorts* on, is not a column. Obsidian builds both its JSON rows and its markdown table from `getOrder()`. |
| The keys of a JSON row | The columns' **display names** | Two columns sharing a display name share one key, and the later one overwrites the earlier — so a column can vanish from the JSON without the row changing shape. |


### The CLI's traps, and where their evidence comes from

These five are the ones that fail SILENTLY: a regression in any of them produces
a result that looks complete and is not. Each is pinned by
`tests/cli_traps.rs`, against the fixture vault `test/fixtures/cli-traps`.

The evidence is the **shipped Obsidian application**, not a live query. The
installation on this host ships its own JavaScript bundle, and the Bases engine
is inside it; reading that file launches nothing, opens no vault and writes
nothing. That matters because the CLI cannot be run here at all — it talks to
the desktop app, which rewrites the `.base` files of the vault it is pointed at,
and the vault the app has open is the parity oracle on `develop`.

The three facts that decide the traps, transcribed from that bundle:

| Trap | The code |
|---|---|
| The columns | `getOrder()` returns `this.order \|\| ["file.name"]`; the CLI maps its rows over it and keys each one by `getDisplayName()`. |
| The values | The cell is `null == n \|\| n === NullValue.value ? "" : n.toString()` — a string, always, with the empty string for an absent one. |
| The sort | The comparator returns `1` when the left value is the null value and `-1` when the right one is, and only THEN applies the direction to the value comparison. |

The live CLI comparison is still owed: it is the deferred UI-validation work
(STATUS.md item 14), and it needs a human in the Obsidian UI. What is here is
the behaviour the shipped code implements, which is stronger than a probe —
it is the code that decides.


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

---

## D12 — A missing argument is an error, but a null argument answers null

| | |
|---|---|
| **Construct** | An argument the call site never passes, next to one it passes as `null`: `status.replace("a")` against `status.replace("a", null)` |
| **Ours** | Three outcomes, as Obsidian has them. A REQUIRED parameter the call never passed raises `Cannot call function "replace", not enough arguments.` A required parameter passed as **null** makes the whole call answer **null**. A parameter marked **optional** that is passed as null is treated as **absent** — so `status.slice(0, null)` is the whole string. |
| **Obsidian** | The same three: absent is an error in a required position, null answers the call null, and null in an optional position is pushed as absent. |
| **Evidence** | Read out of Obsidian's own Bases evaluator, which ships in `resources/obsidian.asar` (`version.json` in that archive says 1.12.4; this file's header records the app as 1.13.7). Its call site checks every argument against the function's parameter table before the body runs — the function is quoted in full below — and the message is Obsidian's own i18n string `msgErrorNotEnoughArguments` from `i18n.js`: `Cannot call function "{{ function }}", not enough arguments.` Pinned by `tests/expr.rs` (`a_missing_required_argument_is_not_enough_arguments`, `a_null_required_argument_answers_null`, `a_null_optional_argument_is_absent`, `a_null_variadic_argument_answers_null`, `a_null_needle_in_a_list_is_compared_rather_than_answering_null`). |
| **verify-in-obsidian** | no — the rule is in Obsidian's shipped evaluator, not in the CLI's rendering of it |

**The checker.** Every method and global is registered with a parameter
table, and the call site runs this before the body:

```js
var S=t.params,x=0,T=[],D=function(e){
  if(x===S.length){ if(!S[S.length-1].variadic) throw new Error(SK.msgErrorTooManyArguments({function:i})); x-- }
  var t=S[x];
  if(t.optional && e===gW.value) T.push(null);          // optional + null  -> absent
  else {
    if(!t.type.some(t=>e instanceof t)){
      if(e===gW.value) return !1;                        // required + null  -> the CALL answers null
      throw new Error(SK.msgErrorTypeError({function:i,parameter:t.name,expected:t.type[0].toString(),given:e.constructor.toString()}))
    }
    T.push(e)
  }
  return x++,!0
};
if(s && !D(s)) return gW.value;                          // the receiver's own argument
for(h=0;h<o.length;h++) if(!D(o[h].getValue(e)||gW.value)) return gW.value;
if(x<S.length) for(h=x;h<S.length;h++) if(!S[h].optional && !S[h].variadic) throw new Error(SK.msgErrorNotEnoughArguments({function:i}))
```

The three outcomes are therefore not stylistic. `e===gW.value` is "this
argument IS the null value"; an argument the call site never wrote never
reaches `D` at all, and the loop over the remaining parameters is what turns
that into the error. `gW.value` is Obsidian's one null singleton, which its
own `NullValue` exposes as `t.value`.

**The bug this repaired.** The Rust implementation read every argument with
`arg(args, i)`, which returned null for an absent argument as well as for a
null one, so the two cases collapsed. Measured on the code this card started
from — `develop` carrying D10's `ce5e764` — with `status` bound to the string
`"active"`:

```
"active".contains()        ERROR Type error in "contains", parameter "value". Expected String not, given null.
"active".slice(0, null)    ""
"active".containsAll(null) false
"active".replace("a")      "ctive"          <- the replacement is ABSENT, not null
```

`replace("a")` is the sharp one: the call is missing its required replacement
and deleted the match instead. In the language Obsidian implements,
`"active".replace("a")` yields `"undefinedctive"` while
`.replace("a", null)` yields `"nullctive"` — the tell that absent and null are
different things, and that neither is an empty string. After this change:

```
"active".contains()        ERROR Cannot call function "contains", not enough arguments.
"active".contains(null)    null
"active".slice(0, null)    "active"
"active".replace("a")      ERROR Cannot call function "replace", not enough arguments.
"active".replace("a", null) null
"active".containsAll(null) null
```

**The parameter tables this models.** Straight out of the binary, where
`zW(name, type...)` is a required parameter, `qW(...)` an optional one and
`jW`/`k([...], jW(...), !0)` a variadic one:

```
contains    [self:String, value:String]
containsAll [self:String, values:String...]            variadic
containsAny [self:String, values:String...]            variadic
startsWith  [self:String, query:String]
endsWith    [self:String, query:String]
repeat      [self:String, count:Number]
slice       [self:String, start:Number, end:Number?]   end optional
split       [self:String, separator:String|RegExp, n:Number?]   n optional
replace     [value:String, pattern:String|RegExp, replacement:String]
list.slice  [self:List, start:Number, end:Number?]     end optional
list.contains [self:List, value (text union)]
```

`replace`'s first parameter is really named `value` in Obsidian (it is the
receiver), which is why the type error for a bad pattern does not name
`self`. Twelve methods read the rule today: the nine String methods above,
`list.contains`, `link.contains` and `list.slice`. Every other built-in still
reads an absent argument as null (`stdlib::arg`), because its parameter table
is not modelled yet; that is the rest of the argument-level ticket, and it is
why `stdlib::arg` still exists with D12 named in its doc comment.

**A null needle in a LIST is compared; in a LINK it answers null.** The two
look identical and are not:

* `list.contains` declares `UW("value","text",mW)` — the `text` union, whose
  type list is `Any`. A null passes the type check, so the call compares it
  instead of answering null. Its body is `operate:function(e,t){return
  e.includes(t)}`, and `EW.prototype.includes` is
  `for(...) if(mW.looseEquals(e,this.get(n))) return!0`, where `get` returns
  the null singleton for a null entry and `NullValue.prototype.equals`
  returns true. So `list([1, 2]).contains(null)` is `false` and
  `list([null]).contains(null)` is **true**.
* a Link is a String in Obsidian — the Link value class is declared with
  `String` as its base, `t}(wW)` — and it registers only `linksTo` and
  `asFile`, so `link.contains` resolves to String's method, whose `value`
  parameter is a plain `String`. Null does not satisfy it, and because the
  null was passed the checker returns the null value:
  `link("Note").contains(null)` is **null**.

**What this corrects in the ticket.** The card had the link case right and the
list case half right: `list.contains(null)` must not answer null, but "false"
is only the answer for a list that does not hold a null — the comparison is
the list's own, and it matches a null entry. Ours answers with `any(item is
null)` for the null needle, which is that same answer, so only the missing
argument had to change there.

(That a Link IS a String also means every String method is callable on a link
in Obsidian. Ours registers only `linksTo`, `asFile` and the null-receiver
fallback on `Link`, so `link("x").slice(1)` is a "not a method on Link" error
here and answers in Obsidian. That is a separate divergence, not this one; it
is recorded here only because it is the reason `link.contains` is governed by
String's parameter table.)

**The TypeScript tree is not repaired.** `ts-implementation` reads arguments
the same way the old Rust code did (`args[0] ?? null` throughout
`src/expr/stdlib.ts`), so it does not model the distinction either. As with
D10, this is the Rust tree's repair; the TypeScript tree is a separate piece
of work.

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

### Date cells lost their `dateOnly` flag (fixed)

`DateValue` carried a `dateOnly` flag, and `file.mtime` was built with it set, so
`toString()` truncated to `YYYY-MM-DD`. `BasesDate` had no flag at all and
rendered full RFC 3339, so a date-only value lost its shape between the two
trees.

`BasesDate` now carries the flag, and `Display` renders a date-only value as
`YYYY-MM-DD` and a datetime as `YYYY-MM-DDTHH:mm:ss` — local, with no offset
suffix, which is what Obsidian's `toString()` does (D11). The flag is
deliberately **not** part of `Eq`/`Ord`/`Hash`: those are written by hand over
the instant, so the query pipeline's sort still compares instants and a
date-only value and a midnight datetime still sort together.

### `list.contains` rejected a Date needle

`m_list_contains` dispatched on the needle's type and rejected everything that
was not a String, Number, Boolean, Link, File, List or null, so
`[date("2026-01-05T17:30")].contains(date("2026-01-05T17:30"))` was a hard
`Type error in "contains", parameter "value". Expected String not, given Date.`
The TypeScript tree answered the same call through a Date arm of its own.

The shipped Obsidian 1.14.4 bundle says neither is right, and reading it
launches nothing. `list.contains` registers
`params: [self: List, value: "text" union over Any]`, so its type check admits
every value, and its body is `operate(e,t){return e.includes(t)}`.
`ListValue.includes` is `for(...) if(aK.looseEquals(e,this.get(n))) return !0`;
the static `aK.looseEquals` falls through to the instances' own `looseEquals`
methods, and `DateValue.looseEquals` compares `date.getTime()` when BOTH sides
carry a time and `this.dateOnly().date.getTime()` otherwise. That is exactly
`dates_equal`: two moments by instant, a day on either side by calendar day.

Fixed by letting every non-null needle take the loose-equality path, which is
what `containsAll` and `containsAny` already did. Pinned by
`contains_reads_a_date_needle_the_way_equality_does`; reinstating the type error
turns its first assertion red.

The TypeScript tree still compares a Date needle by exact milliseconds, so it
answers `false` for a date-only needle against a moment on that day. That arm is
a divergence in the TypeScript tree, not a measurement of Obsidian; the Rust
tree follows the bundle.

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

### `get_note` never returned the `base_hash` its own `write_note` demanded

`write_note` took an optional `base_hash` and refused the write when it did not
match the note on disk — the conditional write that stops an agent's edit from
overwriting whatever arrived since it read. Its argument description says where
the value comes from: "the `base_hash` from the `get_note` this edit is based
on".

`get_note` never returned one. The payload carried `path`, `raw`, `content` and
`regions`, and the `WriteNoteArgs` doc comment — the only place the contract was
written down — described a value the read did not produce. An agent following the
documented flow had nothing to quote, so every write it made was a BLIND write:
`base_hash` omitted, the concurrency check skipped, and the note on disk
overwritten with whatever had arrived since the read.

The hash was never missing from the engine. `Resolver::read_note` computes it
(`content_hash(&raw)`) on both paths, and `NoteView::base_hash` carried it — the
tool simply did not put it in the payload. The TypeScript tree returned it from
the same read, so this was a divergence between the two implementations and not
a design choice.

The cause is visible in the history. `c4af62f` ("Fix four data-loss bugs") added
the conditional write to the Rust tree, and its own message says "`get_note`
returns a `base_hash` and `write_note` takes it" — but the diff adds the
`base_hash` argument to `write_note` and touches nothing in `get_note`'s
payload. The port that added the check did not add the value the check
compares against, and the half that was missing is the half an agent can see.
The TypeScript port of the same fix (`9a86bd5`) added both halves, which is why
the two trees disagreed.

Fixed in `src/tools.rs`: `get_note`'s payload carries `base_hash`, and
`write_note`'s schema declares the argument it has always accepted — the
handler read `base_hash` from a struct field that no declared schema described,
so a client reading `tools/list` had no way to know the argument existed. Both
tool descriptions now name the value and the flow, because the description is
the only channel a model reads.

Pinned by `get_note_returns_the_base_hash_write_note_compares_against`,
`the_projection_carries_the_same_base_hash_as_the_raw_text`,
`the_documented_conditional_write_flow_works_through_the_tool_surface` and
`a_hash_from_get_note_that_has_gone_stale_is_refused` in `tests/tools.rs`. The
last one is the half worth having: it asserts that a note changed between the
read and the write is REFUSED and the user's text survives.

Measured across the two trees, over a real MCP stdio session, each side
pointed at a private copy of the testing vault:

```
before:  DIFF base_hash: rust=None  ts='68f8d427fb15f5083ab15b538f6fc255'
after:   OK   base_hash: rust='68f8d427fb15f5083ab15b538f6fc255'
                                                  ts='68f8d427fb15f5083ab15b538f6fc255'
```

and the whole `get_note` payload — `path`, `raw`, `content`, `base_hash`,
`regions`, `health` — is identical on both sides after the fix, which is what
the "verified equivalent" claim above needs to be true of this tool.

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

**A second case is no longer equivalent, also deliberately.** A method call on
a **null receiver**. `develop` answers null and drops the row (D10); the
TypeScript tree still raises `Type error: "contains" is not a method on null`.
Both are now recorded rather than assumed equal, and the eleven cases above
contain no null-receiver call, so the measurement stands as measured. The
TypeScript repair is a separate piece of work.

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
