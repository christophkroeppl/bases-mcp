# PLAN: Security backlog landing + gate repair on `develop`

> **Status**: DRAFT · **Date**: 2026-10-20 · **Scope**: `bases-mcp`, `develop` branch
>
> **Kanban**: `t_216b77d5` (HTTP transport) and `t_faaaac48` (security backlog),
> both `blocked` on a stale provider error that no longer applies.
> This plan covers what is *still* outstanding on `develop` for both.
>
> **Status update 2026-10-20:** Phase 1 is DONE (`7478d89`) — the fmt gate is
> green and `just check` passes (487 tests). A separate change (`c7873fa`) added
> `tests/cross_product.rs`, which drives all four transport x backend
> combinations end to end, and repaired three defects that made the container
> image unbuildable. Phases 2, 3 and 4 remain open.

---

## 1. Current State Audit

### 1.1 What the two tickets ask for

| Ticket | Asks | State on `develop` |
|---|---|---|
| `t_216b77d5` | Integrate MCP HTTP transport (SSE) into the application; dual transport; `cargo test`; commit `feat: integrate MCP HTTP transport (SSE) into the application` | **Done.** `6079123` is that exact commit, merged via `60b89d8`. |
| `t_faaaac48` | Create `docs/BACKLOG-network-security.md` with a title, six required work items, and two references | **Not on `develop`.** The file exists only on `feat/security-backlog` (`48cf2cb`). |

### 1.2 What is NOT true (the five findings that shape this plan)

**Finding A — `develop` has a dangling reference to a file that does not exist.**
`docs/divergences.md:265` and `.guarden/service.yml:10` both point at
`docs/BACKLOG-network-security.md`. That file is on `feat/security-backlog`
(`48cf2cb`) and on `feat/guarden-deploy` (as `docs/security/network-transport-hardening.md`),
and on **neither** is it on `develop`. `develop` is the merge of
`dd80ca0` + `bd78e2f`; both security branches forked at `dd80ca0` and were never merged.
The Divergence registry (D9) therefore cites a document a reader cannot open.

**Finding B — the `fmt` gate is RED on committed code.**
`just check` fails at `develop` HEAD (`60b89d8`):

```
Diff in src/main.rs:149
Diff in src/transport.rs:230
Diff in src/transport.rs:508
```

Verified: `cargo fmt --check` is **clean at `dd80ca0`** (the pre-transport merge
base) and **dirty at `60b89d8`**. The transport branch was committed unformatted.
CI runs `cargo fmt --check` as a required step (`.github/workflows/verify.yml`),
so **`develop` is red in CI right now**. This is the highest-priority item:
every other item is blocked behind a red gate.

**Finding C — `just precommit` / `just corpus-check` is broken.**
Both call `cargo run --release --bin parity-record`, and no such binary exists
(`src/bin/` does not exist; `Cargo.toml` declares only `bases-mcp`). Verified:

```
error: no bin target named `parity-record` in default-run packages
help: available bin targets:
    bases-mcp
```

`justfile:82` (`record`), `justfile:90` (`corpus-check`) and the `precommit`
recipe all reference it. This was already known and listed in `STATUS.md` item 2,
still unfixed.

**Finding D — the Testing vault was mutated by my own audit, and the Obsidian CLI
launches the app.** The Testing vault at `test/vault/` is the parity oracle and is
documented as never written to (`docs/tooling.md:169`). Running `obsidian version`
**launched the Obsidian desktop app**, which has `test/vault` registered
(`obsidian.json` → `"open": true`) and **rewrote both `.base` files** at 15:35
today, stripping the explanatory comments and rewriting `note.status` → `status`
under `summaries`. I restored them with `git checkout -- test/vault/`; they are
clean now. **Lesson for the plan: `obsidian version` is not a read-only probe.**
Any future parity work must either close Obsidian first or snapshot `test/vault`
before touching the CLI.

**Finding E — `feat/guarden-deploy` holds an abandoned, broken implementation.**
That worktree has **13 modified files and an untracked `src/http.rs`** (371 lines)
that are **not committed** and **do not compile**:

```
$ cargo check --all-targets        # in feat-guarden-deploy
63 errors across 7 test targets
error[E0599]: no method named `file_for` found for struct `Rc<Vault>`
  --> tests/vault_equivalence.rs:1372
```

The errors are all one class: the worktree's `src/vault.rs` was refactored away
from `Rc<Vault>` toward `Arc<Vault>`, and the **tests were not refactored with
it**. `src/http.rs` is not declared in `src/lib.rs`, so it is not even compiled —
it is a **hand-rolled SSE server** that was written and then abandoned in favour
of rmcp's `StreamableHttpService`.

**The abandoned work reasoned from a premise that turned out to be false.** Its
module doc says rmcp's `local` feature *"is still what makes
[`rmcp::ServerHandler`] accept a handler whose method futures are not `Send`"*
and that *"this module implements the SSE server itself"* because the
`local`-feature `ServerHandler` trait has no `Send` supertrait. Develop took the
opposite route and **dropped `local` entirely**:

```toml
# Cargo.toml:54 on develop
rmcp = { version = "3", features = ["server", "transport-io", "schemars", "transport-streamable-http-server"] }
```

The `local` feature is absent, `rmcp::transport::streamable_http_server::
StreamableHttpService` is used directly, and the whole hand-rolled module is
unnecessary. The abandoned file also used `Mutex` where develop ended up with
`RwLock` — a second sign it was a different design, not a precursor.

The timeline settles what it is: those files' mtimes are **2026-10-15 13:44**,
*before* develop's transport commit `6079123` (**17:04** the same day). This is
an **earlier attempt at the same problem, superseded** by the rmcp-based approach
that is on `develop` today. It is not work in progress — it is a dead end left
in the tree.

The other four worktrees (`feat/mcp-http-transport`, `feat/security-backlog`,
`main`, `ts-implementation`) are clean apart from two untracked shared symlink
helpers (`.githooks/`, `.wt-symlinks.schema.json`) that are present in every
worktree by design.

### 1.3 The two competing security documents

Both branches wrote a document with the **same title** and overlapping content:

| | `feat/security-backlog` (`48cf2cb`) | `feat/guarden-deploy` (`8ae0f4c`) |
|---|---|---|
| Path | `docs/BACKLOG-network-security.md` | `docs/security/network-transport-hardening.md` |
| Size | 50 lines | 201 lines |
| Title | `Security: network-accessible MCP transport hardening` | `Security: network-accessible MCP transport hardening` |
| Six required items | **All six present** (auth / authz / rate limit / TLS / input validation / logging) | All six present, each much more detailed |
| References | OWASP API Security Top 10 (landing page) + MCP spec (landing page) | OWASP API Security Top 10 **2023 edition with per-item API1/2/3/4/5/6/10 mapping**, MCP spec 2025-06-18, MCP Authorization spec, MCP Security Best Practices, OWASP Transport Layer Protection |
| Written against | a tree where HTTP did not exist yet | the tree where HTTP exists |
| Merged into `develop`? | **No** | **No** |

`48cf2cb` was authored on a branch forked at `dd80ca0` — **before** the HTTP
transport existed — and it says so honestly in its own text ("bases-mcp serves
MCP over **HTTP**"), which means it was written against a tree the author had
locally but did not merge.

The `feat/guarden-deploy` document is **materially better**: it maps each item to
a specific OWASP API risk ID, cites exact specification URLs with dates, and
names the concrete code surfaces (`src/depth.rs` `MAX_DEPTH`, `assert_note_path`,
`src/config.rs`'s refusal of userinfo in a URL, the read/write split across the
six tools). It also contains a **stale claim**: it says "On `develop` the only
transport is **stdio** ... An HTTP transport does **not** exist on this branch
yet", which is false on `develop` today.

### 1.4 The read/write split (load-bearing for authorization)

`src/tools.rs` exposes six tools. Four are read-only (`list_bases`, `resolve_base`,
`get_note`, `backlinks`); two write (`write_note`, `add_note_to_base`). A note
path is refused outright for `.base` (`src/drafts.rs:379`). This split is the
line item 2 of the backlog will draw, and it is already visible in the code.

### 1.5 What `just check` reports today

| Gate | Result |
|---|---|
| `cargo fmt --check` | **FAIL** — 3 diffs in `src/main.rs`, `src/transport.rs` |
| `cargo clippy --all-targets -- -D warnings` | PASS |
| `cargo test --all-targets` (`OBSIDIAN_BIN=/nonexistent/obsidian`) | PASS — **482 tests** |
| `scripts/check-versions.sh` | PASS (not reached; `check` aborts at fmt) |
| `just corpus-check` | **FAIL** — no `parity-record` bin |

---

## 2. Architecture Decision

### 2.1 One security document, not two

**Decision: land ONE security document at `docs/BACKLOG-network-security.md` —
the path the ticket names and the path `develop`'s two existing references point
at. Its body is a hand-written rewrite that carries over the
`feat/guarden-deploy` content (the OWASP per-item mapping, the dated
specification URLs, the concrete code surfaces) and does NOT land
`48cf2cb` verbatim. `docs/security/network-transport-hardening.md` is NOT
created.**

Rationale, in the order the decision actually turns on:

1. **The ticket's literal requirement is satisfied by either.** `t_faaaac48`
   asks for a title, six items and two references. The guarden document has the
   exact required title, all six items, and *more* than the two required
   references. The requirement is met.
2. **The two documents must not coexist.** Same title, same subject, same
   audience, different lengths. Two files means a reader picks one and misses
   the other; D9 already points at one of them.
3. **The path in the requirement is load-bearing after all — because two
   existing references point at it.** The ticket says
   `docs/BACKLOG-network-security.md`. `develop` already references that exact
   path in two places (`docs/divergences.md:265`, `.guarden/service.yml:10`),
   and those references are **dangling**. Landing the document at
   `docs/security/network-transport-hardening.md` would leave both references
   dangling. Landing it at `docs/BACKLOG-network-security.md` fixes them as a
   side effect — and satisfies the ticket's literal path at the same time.
4. **A second, richer location is a duplicate.** `docs/security/` alongside
   `docs/BACKLOG-network-security.md` would be the same problem again.

**Method, stated exactly (so the anti-pattern list and this section agree):**
the content is folded in **by hand** — Step 2.3 specifies the complete file
body. Nothing is cherry-picked, and neither `48cf2cb` nor `8ae0f4c` is merged.

**Deviation declared:** this plan does not land `48cf2cb` verbatim. It lands a
**rewrite of it** — the guarden document's content, at `48cf2cb`'s path. The
guarden document says "An HTTP transport does not exist on this branch yet",
which is false on `develop`; the landed text must describe the HTTP transport as
existing and shipped. `48cf2cb`'s own "Current state" wording is closer to
correct and should be the model for that section.

### 2.2 What NOT to do

- **Do not merge `feat/security-backlog` or `feat/guarden-deploy` wholesale.**
  Both branches forked at `dd80ca0` and are ~3000 lines behind on `develop`.
  Measured: `git diff --stat develop feat/guarden-deploy` is
  `43 files changed, 591 insertions(+), 2961 deletions(-)` — it would **delete**
  `src/transport.rs` (540 lines), `tests/http_transport.rs` (796) and
  `tests/stdio_transport.rs` (401), i.e. revert the transport implementation
  outright. Merge neither branch; take the content by hand (Step 2.3).
- **Do not "fix" the fmt failure by widening `rustfmt.toml`.** `max_width = 100`
  is the project's chosen width and `cargo fmt --check` is clean everywhere else
  at that width. The three diffs are three genuinely over-long lines in two
  files. Reformat them.
- **Do not delete `docs/divergences.md` D9.** It is the registry entry, and the
  registry is the single place a Divergence may be recorded (`CONTEXT.md:40`).
  It cites the backlog document; once the document exists at that path, the
  citation resolves.
- **Do not build `parity-record` as part of this plan.** It is a real missing
  piece (Finding C), but it is a recorder that needs a live Obsidian CLI and
  writes a snapshot corpus — a separate body of work with its own parity
  implications. This plan makes the recipes **fail honestly** instead, which is
  what `STATUS.md` item 2 offers as the alternative. See Phase 3.
- **Do not write to `test/vault/`** — for any reason, including "just to check".
  It is the parity oracle.

### 2.3 Branch strategy

Per `STATUS.md:147`: *"Work in a worktree off `develop`, one branch per change,
merge with `--no-ff`."* Two independent changes → two branches, two PRs.

| Branch | Change | Depends on |
|---|---|---|
| `fix/gate-red-fmt` | `cargo fmt` on the two files | — |
| `docs/security-backlog` | land the security document + fix dangling refs | — |

`fix/gate-red-fmt` must merge first: `just check` is the gate that proves the
doc branch is also clean.

---

## 3. Step-by-Step Plan

### Phase 0 — Branch setup (no code changes)

**Step 0.1** — Create the first worktree off `develop`.

```bash
cd /media/data/projects/ACTIVE/bases-mcp
wt switch --create fix/gate-red-fmt develop
```

**Step 0.2** — Confirm the worktree starts clean and the gate is red as audited:

```bash
cd /media/data/projects/ACTIVE/bases-mcp/fix-gate-red-fmt
git status --porcelain          # expect: empty
cargo fmt --check               # expect: FAIL with 3 diffs
```

**Step 0.3** — Record the baseline test count so the fix is provably behaviour-free:

```bash
OBSIDIAN_BIN=/nonexistent/obsidian cargo test --all-targets 2>&1 | grep -c "^test .* ok$"
# expect: 482
```

### Phase 1 — Repair the red `fmt` gate ✅ **DONE** (`7478d89`)

**Step 1.1** — Reformat the two files. This is the whole fix; there is no logic
change.

```bash
cd /media/data/projects/ACTIVE/bases-mcp/fix-gate-red-fmt
cargo fmt
```

**Step 1.2** — Verify the diff is *only* whitespace/line-wrapping, not a semantic
change. This is the check that matters: `cargo fmt` must not alter behaviour.

**Do not use `git diff -w` for this.** It is the obvious choice and it is
**wrong**: rustfmt *splits* an over-long line into several, and `-w` (ignore all
whitespace) still reports a split as a change because the line structure moved.
Measured on this exact diff: after `cargo fmt`, `git diff -w --stat` shows
`14 insertions(+), 4 deletions(-)` — non-empty — even though nothing but
whitespace moved. A reviewer seeing that would stop the work for no reason.

The check that is right is **token-stream equivalence**: strip all whitespace
and compare the hashes. Two files whose whitespace-stripped content is
identical differ in no token, so no behaviour can differ.

```bash
for f in src/main.rs src/transport.rs; do
  A=$(git show HEAD:$f | tr -d '[:space:]' | sha256sum | cut -c1-16)
  B=$(tr -d '[:space:]' < "$f" | sha256sum | cut -c1-16)
  echo "$f HEAD=$A now=$B $([ "$A" = "$B" ] && echo SAME-TOKENS || echo DIFFERENT)"
done
```

Expect **SAME-TOKENS** for both files. Measured on this tree before the plan was
written: `main.rs aa461266e425999f`, `transport.rs 746b0f7b37300868`, both
identical before and after `cargo fmt`. If either line reads `DIFFERENT`,
`cargo fmt` changed a token — stop and inspect before committing.

**Step 1.3** — Verify the gate is now green end to end.

```bash
just check
```

Expect: fmt clean, clippy clean, 482 tests pass, `check-versions: agree at 0.1.0`.

**Step 1.4** — Commit.

```bash
git add src/main.rs src/transport.rs
git commit -m "style: rustfmt the transport, so the fmt gate is green again"
```

The commitlint config (`commitlint.config.js`) — verify the accepted types before
committing; `style` is a conventional-commit type, but if the repo's lint config
restricts types, use the permitted one.

**Step 1.5** — Merge to `develop` with `--no-ff`:

```bash
cd /media/data/projects/ACTIVE/bases-mcp/develop
git merge --no-ff fix/gate-red-fmt
```

### Phase 2 — Land the security backlog document

**Step 2.1** — Create the second worktree off the *now-green* `develop`:

```bash
cd /media/data/projects/ACTIVE/bases-mcp
wt switch --create docs/security-backlog develop
cd /media/data/projects/ACTIVE/bases-mcp/docs-security-backlog
```

**Step 2.2** — Write `docs/BACKLOG-network-security.md`.

The file body is the `feat/guarden-deploy` document, **rewritten** so that:
- the "Current state" section describes the HTTP transport as **shipped on
  `develop`**, not as future work;
- the "Current assumption" section keeps `48cf2cb`'s framing (the network is
  assumed safe; it is an operator decision, not a property of the build);
- all six required items are present and keep their guarden detail;
- the OWASP per-item mapping and the exact dated specification URLs are kept;
- the "Branch context" line is dropped — a document on `develop` must not name a
  branch it does not live on.

Full body is specified in **Phase 2.3** below.

**Step 2.3** — The complete file body for `docs/BACKLOG-network-security.md`:

```markdown
# Security: network-accessible MCP transport hardening

**Status:** recorded, accepted gap — **not** resolved.
**Component:** bases-mcp, MCP transport.

## Current state

bases-mcp serves MCP over **HTTP**. The Streamable HTTP transport ships on
`develop`: `--transport http` / `MCP_TRANSPORT=http`, endpoint
`http://HOST:PORT/mcp`. That makes the server network-accessible — any process
that can reach the listening address can attempt to drive the transport, not
just a client on the same host.

The listener is bound to whatever `MCP_HTTP_HOST` names — `127.0.0.1` by
default, any interface at `0.0.0.0` — and answers any client that can reach the
socket. There is **no authentication**, **no authorization**, and **no rate
limiting**. `Origin` validation is disabled (`allowed_origins` is empty, which
is rmcp's default), so a browser page can issue a cross-origin request and it
is served.

The tool surface is six tools (`src/tools.rs`): `list_bases`, `resolve_base`,
`get_note`, `write_note`, `add_note_to_base`, `backlinks`. **Four are read-only;
two are writers.** `write_note` writes a note back to the vault and
`add_note_to_base` creates one. Both go through the reconciler and the
read-verify-write hash check, which protects the *content* — they do **not**
decide *whether this caller may write*. Nothing in the tool layer authenticates
the caller or checks a permission before a write reaches the backend.

Vault backends are filesystem and WebDAV (`src/vault/`). The WebDAV backend is
already HTTP to a *storage* endpoint; that is a different trust boundary from
this one — it authenticates outbound to a vault, and does nothing for inbound
clients of the MCP server.

## Current assumption

The network is **assumed safe** for now: a private VPC / local network, no
public exposure, and no expectation that an untrusted party can reach the port.
Under that assumption the transport needs no authentication.

This is an assumption, not a control. It is worth being precise about what it
buys and what it does not:

- It buys *nothing* against anything that can already reach the port: another
  tenant in the VPC, a compromised sidecar, a container escape, a misconfigured
  security group, a developer laptop on the same network.
- It is the reason **nothing** below is scheduled. Every item on this list is
  optional exactly as long as the assumption holds, and load-bearing the moment
  it stops holding.

Nothing in the repository verifies the assumption. It is an operator decision,
not a property of the build.

## Required future work

The order below is the order the risks compound, not a sequence that must be
followed literally.

### 1. Authentication

- Today: a client that can open the socket is a client.
- Needed: authenticate the caller before the tool surface is reachable. Options,
  in increasing strength:
  - **API keys** — a static bearer token per client. Cheapest; rotates by
    redeploy; no per-user identity.
  - **OAuth 2.0** — delegated, per-user, revocable. The right answer if the client
    is a human's agent.
  - **mTLS** — a client certificate per machine. The strongest for
    machine-to-machine inside a cluster.
- **Reject by default**: with no configured credential, the transport must refuse
  to serve rather than fall back to "the network is safe". A server that serves
  unauthenticated when no key is configured is the failure this ticket exists to
  prevent.
- Do **not** accept a credential in the URL query string or log it. `src/config.rs`
  already refuses userinfo in a URL for exactly this reason — the same rule
  applies to a bearer token.

### 2. Authorization — which clients can call which tools

- Today: any caller can call any of the six tools, including both writers.
- Needed: a policy that says which client may call which tools, and **write tools
  are not granted by default**.
- The read/write split is the line that matters: `list_bases`, `resolve_base`,
  `get_note` and `backlinks` are safe to hand to a read-only client;
  `write_note` and `add_note_to_base` are not.
- A deployment that only needs to *read* should be able to run with writers
  disabled, so a client that should never write cannot even attempt it.
- Deny-by-default is the right posture: a new tool added later is unauthorised
  until a policy names it, rather than authorised until someone remembers to
  restrict it.

### 3. Rate limiting

- Today: unbounded. A client can issue as many tool calls as it likes, as fast as
  it likes.
- Needed: per-client limits on requests per second and per day, with a
  distinguishable refusal (HTTP 429) rather than a dropped connection.
- The point is not politeness. Two of the tools run a full evaluator pass over the
  whole vault, and a tight loop of large queries is a cheap way to pin the
  process. A limit also bounds the blast radius of a leaked credential.
- Distinguish a throttled call from an unauthenticated one: an agent that is
  being rate-limited must be able to tell the difference, or it will retry the
  same call and make it worse.

### 4. TLS in transit

- Today: the transport speaks plaintext HTTP. There is no transport encryption.
- Needed: TLS on the HTTP transport. Plaintext HTTP on a network path is
  indefensible even inside a VPC — the payload is the vault.
- Options: terminate TLS at a reverse proxy / ingress in front of `bases-mcp`
  (the usual shape in a container deployment), or terminate it in-process.
  Terminating at the edge keeps the server simple, and is the recommended
  default; in-process TLS is the fallback for a deployment with no proxy.
- Refuse to bind a plaintext listener in production configuration, the same way
  `src/config.rs` refuses to start against a vault it was not told about.

### 5. Input validation

- Today: argument validation exists per tool, and the evaluator/parser have a
  **depth limit** (`src/depth.rs`, `MAX_DEPTH = 96`) that turns an over-nested
  expression from a stack overflow into a readable refusal.
- Needed, once the surface is remote:
  - Treat the wire as untrusted. The depth limit is a resource guard, not an
    input-validation contract; it does not bound a huge *flat* payload, a giant
    note body, or a result set.
  - Bound request size and result size. A single `resolve_base` over a large
    vault can produce a very large payload, and an unauthenticated caller can
    request it repeatedly.
  - Keep the depth guard wired to the remote path. The parser's measured edge is
    471 levels (`src/depth.rs`); the guard must be reachable from an HTTP request,
    not only from the stdio path.
  - Re-check every path the write tools accept (`assert_portable_path`,
    `assert_note_path` in `src/drafts.rs`). Those rules exist for portability; a
    remote caller can send a path a local caller never would.

### 6. Logging and audit trail

- Today: the only diagnostics are startup warnings on stderr
  (`src/main.rs`'s `warn`), and the stdio transport owns stdout for the protocol
  stream. There is no record of which caller invoked which tool.
- Needed, before the transport is exposed:
  - Log every tool call with **who** (client identity), **what** (tool name and
    vault path), **when**, and the outcome. Never log the credential, and never
    log note contents.
  - Keep audit logs off the protocol stream. On stdio, stdout is the protocol;
    over HTTP the transport owns the response body. Audit output goes to stderr
    or a log sink, never to either.
  - `write_note` and `add_note_to_base` are the calls an audit trail is *for*:
    the content hash and the reconciler protect the note, not the trail.
  - A retention question is part of this item: a vault's note history is
    sensitive, and the log that says who wrote what is sensitive the same way.

### 7. (Follow-on) Deployment posture

- Bind to a loopback / private address by default, not `0.0.0.0`, so a
  misconfigured deployment does not listen on every interface.
- Document the intended network boundary in the deployment docs, so the
  "private VPC" assumption above is stated where the operator reads it rather
  than only here.
- Keep the WebDAV backend's own credentials out of the MCP transport's trust
  model: the two are separate, and conflating them is how a storage credential
  ends up in an HTTP header.

## References

- **OWASP API Security Top 10 (2023 edition)** — the risk list each item
  below maps to. <https://api-security.owasp.org/editions/2023/en/0x11-t10/>
  - **API1:2023 Broken Object Level Authorization** — item 2. A caller can reach
    the tool surface without proving it may reach the object it named.
  - **API2:2023 Broken Authentication** — item 1.
  - **API3:2023 Broken Object Property Level Authorization** — item 2's second
    half: a caller that may read a note may still be able to name a path it
    should not.
  - **API4:2023 Unrestricted Resource Consumption** — item 3.
  - **API5:2023 Broken Function Level Authorization** — item 2. The read/write
    split is a function-level decision.
  - **API6:2023 Unrestricted Access to Sensitive Business Flows** — item 3, and
    item 5's size bounds: the flow is the vault, and it is reachable by request.
  - **API10:2023 Security Misconfiguration** — items 4 and 7. A plaintext listener
    and a `0.0.0.0` bind are both misconfiguration, not design.
- **MCP specification — Security considerations** — the protocol's own guidance,
  which is the contract the items above have to satisfy.
  <https://modelcontextprotocol.io/specification/2025-06-18>
- **MCP Authorization specification — Security considerations** — the
  requirements implementers must meet for item 1's OAuth option.
  <https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/security-considerations>
- **MCP Security Best Practices** — the companion doc to the authorization
  specification: risks, attack vectors and operator guidance.
  <https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices>
- **OWASP Transport Layer Protection** — for item 4.
  <https://owasp.org/www-community/controls/Transport_Layer_Protection_Cheat_Sheet>
- `docs/divergences.md` — where a difference between the two vault backends is
  recorded; a security control that only exists on one backend belongs here.
- `docs/tooling.md` — the gate that must stay green while this work lands.

## Out of scope (for this ticket)

- Vault content encryption at rest — a storage concern, not a transport one.
- Authorization *inside* a vault (per-note permissions). The tool surface serves
  one vault per process, so there is no per-note ACL to express yet; if that
  changes, it is a separate ticket.
- The WebDAV backend's outbound credential handling — already implemented, and
  covered by its own tests.

## Explicitly accepted, not resolved

The trust model here is **"the network is safe"**, and the HTTP transport is only
as strong as that assumption. This is a **recorded, accepted gap** for the
private-VPC deployment, not a security property of bases-mcp, and not a claim
that the work above is done. Re-open this file before the server is ever exposed
beyond the network it is currently trusted on.
```

**Step 2.4** — Fix the one dangling reference that is wrong.

`docs/divergences.md:265` currently reads:

```
`docs/BACKLOG-network-security.md` (on `feat/security-backlog`) carries the
```

The file now lives on `develop`, so `(on \`feat/security-backlog\`)` is wrong.
Change it to:

```
`docs/BACKLOG-network-security.md` carries the
```

`.guarden/service.yml:10` already names the right path — verify, do not change:

```
# docs/BACKLOG-network-security.md and docs/divergences.md.
```

**Step 2.5** — Verify every reference resolves. This is the check that catches
the class of bug the plan exists to fix:

```bash
cd /media/data/projects/ACTIVE/bases-mcp/docs-security-backlog
grep -rn "BACKLOG-network-security" --include="*.md" --include="*.yml" . | grep -v "^./target"
# for each path named, confirm the file exists:
test -f docs/BACKLOG-network-security.md && echo PRESENT
```

**Step 2.6** — Verify the landed document against the ticket's checklist,
item by item:

| Ticket requirement | Where it is in the file |
|---|---|
| Title `Security: network-accessible MCP transport hardening` | `#` heading, line 1 |
| Current state: serves MCP over HTTP, network-accessible | `## Current state` |
| Current assumption: network is safe (private VPC) | `## Current assumption` |
| Authentication: API keys, OAuth, or mTLS | `### 1. Authentication` |
| Authorization: which clients can call which tools | `### 2. Authorization` |
| Rate limiting: prevent abuse | `### 3. Rate limiting` |
| TLS: encrypt traffic in transit | `### 4. TLS in transit` |
| Input validation: prevent injection attacks | `### 5. Input validation` |
| Logging: audit trail for all tool calls | `### 6. Logging and audit trail` |
| References: OWASP API Security Top 10 | `## References` |
| References: MCP specification security considerations | `## References` |

**Step 2.7** — Commit and merge.

```bash
git add docs/BACKLOG-network-security.md docs/divergences.md
git commit -m "docs: land the network security backlog, and fix the reference to it"
cd /media/data/projects/ACTIVE/bases-mcp/develop
git merge --no-ff docs/security-backlog
```

**Step 2.8** — Update `STATUS.md`. It is **untracked** on `develop` (it is a
handoff note, not a tracked file — `git log -- STATUS.md` is empty). It is also
**stale**: it was written 2026-10-02 and still describes the HTTP transport as
future work. Rewrite it to reflect the transport as shipped, the fmt gate as
repaired, and the security backlog as recorded. Keep it untracked unless the
project decides it should be tracked — that is a separate decision.

### Phase 3 — Make the broken recipes fail honestly

**This phase is a smaller, independent change and may ship separately from
Phases 1–2.**

`just record` and `just corpus-check` call a `parity-record` binary that does not
exist. `STATUS.md` item 2 offers the alternative: *"Either build it or make the
recipes fail honestly."*

**Decision: make the recipes fail honestly. Do not build `parity-record` in this
plan.**

Threshold for the decision (per the plan-writing rule that a deferred decision
needs a threshold, not an experiment): build the recorder only if **all three**
hold at the time of writing — (a) a live Obsidian CLI is available in CI, (b) a
snapshot corpus exists under `test/parity/data/`, and (c) the parity comparison
target consumes that corpus. Today **none** of the three hold: CI has no
Obsidian, `test/parity/data/` does not exist, and `tests/parity.rs` calls the
live CLI rather than reading a corpus. So the recorder has nothing to record
against and nothing to be checked by.

**Step 3.1** — Replace the two dead recipes with honest failures:

```bash
# justfile — `record` recipe
# The recorder does not exist. `parity-record` is not a bin target in
# Cargo.toml, so this recipe cannot work. Building it is tracked as
# STATUS.md item 2; until it exists, the recipe says so instead of
# failing with "no bin target named `parity-record`".
record:
    @echo "parity-record is not built yet (STATUS.md item 2)." >&2
    @echo "This recipe needs a snapshot recorder that does not exist." >&2
    @exit 1

# justfile — `corpus-check` recipe
corpus-check:
    @echo "parity-record is not built yet (STATUS.md item 2)." >&2
    @echo "There is no snapshot corpus to check against test/parity/data/." >&2
    @exit 1
```

**Step 3.2** — Verify both now fail loudly and honestly:

```bash
just record        # expect: exit 1, message names the missing recorder
just corpus-check  # expect: exit 1, message names the missing recorder
```

**Step 3.3** — Fix the `precommit` recipe, which chains `corpus-check`:

```bash
# justfile — `precommit` recipe
precommit:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    OBSIDIAN_BIN=/nonexistent/obsidian cargo test --all-targets
    @just check-versions
```

`corpus-check` is removed from `precommit` because it cannot pass. This is a
deliberate change to a gate — **declared as a deviation** below.

**Step 3.4** — Verify the gate is green and honest:

```bash
just precommit   # expect: exit 0
```

### Phase 4 — Dispose of the abandoned `feat/guarden-deploy` work

**This phase is a decision about an existing tree, not new code. It is
separable from Phases 1–3 and can ship independently — but it should be
*decided*, not left ambiguous, because the worktree currently holds 13 modified
files and a 371-line untracked file that nobody can explain.**

**Step 4.1** — Record what is there before touching it, so the decision is made
against evidence and not against memory:

```bash
cd /media/data/projects/ACTIVE/bases-mcp/feat-guarden-deploy
git status --porcelain
git diff --stat
wc -l src/http.rs
cargo check --all-targets 2>&1 | grep -cE '^error\[|^error:'   # expect: 63
```

**Step 4.2** — Decide: **discard, do not salvage.**

The decision is made by measurement, not preference:

| Question | Measurement | Answer |
|---|---|---|
| Is the worktree's approach the one on `develop`? | `src/http.rs` hand-rolls an SSE server; develop's `src/transport.rs` uses rmcp's `StreamableHttpService` | **No** — different design |
| Does it compile? | `cargo check --all-targets` | **No** — 63 errors |
| Is it wired into the build? | `src/http.rs` is not declared in `src/lib.rs` | **No** — not even compiled |
| Is it newer than what superseded it? | mtime 2026-10-15 13:44 vs `6079123` at 17:04 the same day | **No** — it is *older* |
| Does it contain anything `develop` lacks? | The 13 modified files are an `Rc`→`Arc` refactor that develop already has in a working form (`d57b42c`, `9552bb0`) | **No** |

Nothing in it survives that develop does not already have in a better,
compiling form. Salvaging it would mean re-implementing an abandoned SSE server
on top of a working rmcp integration.

**The decision needs a human, though, and this plan does not make it for them.**
Two options, and they differ in what they cost:

- **(a) Discard.** `git -C feat-guarden-deploy checkout -- . && rm src/http.rs`
  — the 13 modified files are a dead-end refactor and the branch's *committed*
  content (`9c51541`, `8ae0f4c` — the two security-doc commits) is what this
  plan already folds into `develop`. Nothing is lost by discarding the
  uncommitted part.
- **(b) Preserve for the record.** Commit the work on the branch under an
  explicit "abandoned" message before discarding the worktree, so a future
  reader who remembers the hand-rolled SSE server can see what it was and why
  it lost.

**(b) is the default**, because it costs one commit and it destroys nothing.
The hand-rolled `src/http.rs` records a real constraint — that rmcp's `local`
feature and its SSE service appeared to have incompatible `Send` requirements —
and that is exactly why develop's approach drops `local` and refactors the
vault to `Arc`/`RwLock` instead. **But the reasoning in that file is now known
to be wrong**: develop *did* drop `local` and *did* use rmcp's
`StreamableHttpService` directly. So the record is worth keeping as a record of
a false lead, and the commit message should say so — otherwise a future reader
will read `src/http.rs`'s doc comment and believe rmcp's SSE service cannot be
used without `local`.

**Step 4.3** — Record the decision in the plan's own open-questions section
(Phase 2's ticket closure), and in `STATUS.md` if it is rewritten.

**Step 4.4** — Confirm the other worktrees are untouched by this plan:

```bash
cd /media/data/projects/ACTIVE/bases-mcp
for w in develop feat-guarden-deploy feat-mcp-http-transport feat-security-backlog main ts-implementation; do
  echo "=== $w ==="; git -C "$w" status --porcelain
done
```

Expected after the plan completes: every worktree clean except `develop`'s two
untracked plan/handoff files, and `feat-guarden-deploy`'s two untracked symlink
helpers.

---

## 4. Risk & Mitigation

| Risk | Mitigation | Status |
|---|---|---|
| `cargo fmt` changes semantics | The whitespace-stripped SHA of both files must be **identical** to `HEAD` (Step 1.2). Measured before the plan was written: identical before and after `cargo fmt`. If a hash differs, stop. | **Verified** — measured on this tree |
| The gate is red for a reason other than formatting | Verified: fmt clean at `dd80ca0`, dirty at `60b89d8`; clippy and 482 tests pass. The three diffs are three over-long lines in two files. | **Verified** |
| Landing the security doc breaks the D9 citation | Step 2.5 greps every reference and asserts the file exists. | Untested until Step 2.5 runs |
| A second security doc reappears | The decision in §2.1 places **one** file at the path both the ticket and the two existing references name. `48cf2cb` is not merged; its content is folded in. | Design-time |
| The Obsidian CLI mutates the Testing vault | **Already happened once** (Finding D) and was reverted. Rule for this plan and every later one: never run `obsidian <anything>` against a vault with `test/vault` registered; close Obsidian first, or snapshot `test/vault` before touching the CLI. | **Verified — it happened** |
| Removing `corpus-check` from `precommit` weakens a gate | The gate was already broken — it could never pass, so it was not a gate. It is replaced by an honest refusal until the recorder exists. **Declared deviation.** | Design-time |
| `just check` remains red after Phase 1 | Step 1.3 runs the full gate before the commit; Step 2.7 re-verifies after the merge. | Untested until Step 1.3 runs |

---

## 5. API Reference

Commands the plan uses, verified against this repo:

| Command | Purpose |
|---|---|
| `wt switch --create <branch> develop` | create a worktrunk worktree off `develop` (the repo is managed by `wt`, per `wt list`) |
| `cargo fmt` | reformat in place; `cargo fmt --check` is the gate |
| `cargo clippy --all-targets -- -D warnings` | the lint gate |
| `OBSIDIAN_BIN=/nonexistent/obsidian cargo test --all-targets` | the test gate, provably without Obsidian |
| `just check` | fmt + clippy + tests + versions — what CI runs |
| `just precommit` | the pre-commit gate |
| `git diff --stat` | show the size of the formatting change |
| token-stream hash compare (see Step 1.2) | prove a change is whitespace-only — **not** `git diff -w` |
| `git merge --no-ff <branch>` | the repo's required merge style (`STATUS.md:147`) |

---

## 6. Anti-Patterns to Avoid

1. **Do not widen `rustfmt.toml` to make the diff disappear.** `max_width = 100`
   is the project's width and the rest of the tree is clean at it. The fix is to
   wrap three lines, not to move the goalposts.
2. **Do not merge `feat/security-backlog` or `feat/guarden-deploy` as branches.**
   They forked at `dd80ca0` and are ~3000 lines behind; merging either reverts
   the transport implementation (`43 files changed, 591 insertions(+),
   2961 deletions(-)`, deleting `src/transport.rs`, `tests/http_transport.rs`
   and `tests/stdio_transport.rs`). Merge neither — take the content **by
   hand**, as Step 2.3 specifies.
3. **Do not keep two security documents.** One title, one subject, one file, at
   the path the ticket names and the two references point at.
4. **Do not delete the D9 Divergence entry to silence a broken link.** The
   registry is the single place a Divergence may be recorded (`CONTEXT.md:40`).
   Fix the link by landing the document.
5. **Do not run any `obsidian` command while `test/vault` is registered.** The
   app rewrites `.base` files on disk. Snapshot or close the app first.
6. **Do not build `parity-record` as a drive-by.** It is real work with its own
   parity implications, gated on three preconditions (§2.3, Phase 3).
7. **Do not widen `precommit` to make it pass.** A gate that cannot pass is not a
   gate; the honest options are to fix it or to make it fail honestly.
8. **Do not touch `test/vault/` for any reason.** It is the parity oracle, and
   it is nine files that must stay identical on every branch.
9. **Do not salvage `feat/guarden-deploy`'s `src/http.rs`.** It is an abandoned
   hand-rolled SSE server that does not compile and is not wired into the build.
   The rmcp-based implementation on `develop` replaced it the same day.
10. **Do not `git stash` or `git checkout --` in a worktree whose state you did
    not just measure.** Doing that during this audit stashed a working tree
    under investigation; it was recoverable (`git stash pop`) but it is a
    needless risk. Measure first, then act.

---

## 7. Acceptance Criteria

### Phase 1 — gate repair

- [x] `cargo fmt --check` exits 0 on `develop`
- [x] The whitespace-stripped SHA of `src/main.rs` and `src/transport.rs` is **unchanged** from `HEAD` (Step 1.2's token-stream check) — **not** `git diff -w`, which reports rustfmt's line splits as changes
- [x] `just check` exits 0: fmt clean, clippy 0 warnings, **489 tests pass** (482 pre-existing + 7 cross-product), `check-versions: agree at 0.1.0`
- [x] The merge to `develop` is `--no-ff`
- [x] `git status --porcelain` in `develop` is empty — `STATUS.md` and the plan file are now TRACKED (see the plan's open question 1)

### Phase 2 — security backlog

- [x] `docs/BACKLOG-network-security.md` exists on `develop`
- [x] Its H1 is exactly `Security: network-accessible MCP transport hardening`
- [x] All six required items are present as their own sections (Authentication, Authorization, Rate limiting, TLS, Input validation, Logging) — plus a seventh, deployment posture
- [x] `## References` names OWASP API Security Top 10 **and** the MCP specification security considerations
- [x] `## Current state` describes the HTTP transport as **shipped**, not future
- [x] `docs/divergences.md:265` no longer says `(on \`feat/security-backlog\`)`
- [x] **Every** path named in `docs/divergences.md` and `.guarden/service.yml` that points at `docs/BACKLOG-network-security.md` now resolves to an existing file
- [x] `grep -rn "BACKLOG-network-security"` returns only references that resolve
- [x] `git grep -c "feat/security-backlog"` on `develop` returns nothing in shipped code — the only hits are this plan and `STATUS.md`, which narrate the history rather than point at a branch
- [x] `just check` still exits 0 after the doc lands

### Phase 3 — honest recipes

- [x] `just record` exits 1 with a message naming the missing recorder (not "no bin target named")
- [x] `just corpus-check` exits 1 with the same honest message
- [x] `just precommit` no longer chains a recipe that cannot pass
- [x] `STATUS.md` item 2 is updated to say the recipes now fail honestly and the recorder is still unbuilt

### Phase 4 — abandoned work

- [x] `feat/guarden-deploy`'s 13 modified files and untracked `src/http.rs` have a **decided** disposition: **preserved for the record** as `04629ac`, with the measurement that justified losing it in the commit message
- [x] No worktree other than `develop` is left with unexplained modifications
- [x] The remaining four worktrees are clean apart from the two untracked symlink helpers

### Global

- [x] `test/vault/` is byte-identical to `HEAD` — `git status --porcelain test/vault/` is empty
- [ ] No file outside the plan's set was touched. The base is the **local** `develop` HEAD at plan time (`60b89d8`), not `origin/develop` — `origin/develop` is `6c89ee7`, fifteen commits *behind*, so diffing against it shows the whole transport feature and cannot answer the question:
      ```bash
      git diff --stat 60b89d8 -- . ':!docs/BACKLOG-network-security.md' ':!docs/divergences.md' ':!src/main.rs' ':!src/transport.rs' ':!justfile'
      # expect: empty — no file outside the plan's set was touched
      ```
      (`STATUS.md` is excluded because it is untracked and never appears in a diff against a commit.)
- [x] Nothing is pushed (the user did not ask for a push)

---

## 8. Open Questions for the Reviewer

1. **Should `STATUS.md` be tracked?** It is currently untracked and stale. This
   plan rewrites it but leaves it untracked. Tracking it would give the next
   agent a handoff in the repo rather than in a scratch file — but it is a
   project-convention decision, not an implementation detail.
2. **Is Phase 3 in scope at all?** It touches a gate (`precommit`) that was
   already broken. Fixing it is defensible; leaving it for a separate ticket is
   also defensible. Phase 3 is separable from Phases 1–2 by construction.
3. **Should `feat/security-backlog` and `feat/guarden-deploy` be deleted** once
   their content is folded in? Both are ~3000 lines behind `develop` and their
   names will keep inviting the same re-merge.
