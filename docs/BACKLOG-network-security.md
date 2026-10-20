# Security: network-accessible MCP transport hardening

**Status:** backlog · not scheduled
**Component:** transport / deployment
**Applies to:** `develop`

## Summary

`bases-mcp` serves MCP over **Streamable HTTP** as well as stdio, and it is
**containerised** (`Containerfile`, `compose.yaml`). Once the HTTP transport is
reachable, anyone who can open a TCP connection to it can speak MCP to it, and
the tool surface is not all read-only. This document records what is accepted
today, what is explicitly **not** defended yet, and the work that has to land
before the assumption below stops being load-bearing.

## Current state

- `bases-mcp` is a Rust MCP server built on `rmcp` (see `Cargo.toml`).
- The HTTP transport **ships**. `src/main.rs:87` `serve_http()` mounts rmcp's
  `StreamableHttpService` on an axum router at `/mcp`; `src/main.rs:68`
  `serve_stdio()` serves the same application over stdio. One `Arc<Resolver>` is
  shared by every session, so all sessions see one vault.
- The listener binds **127.0.0.1 by default** (`MCP_HTTP_HOST`), and the
  containerised deployment overrides it to `0.0.0.0` (`compose.yaml`). So the
  *default* is loopback and the *exposed* case is a deliberate, documented
  opt-in.
- The tool surface is six tools (`src/tools.rs:134-139`): `list_bases`,
  `resolve_base`, `get_note`, `write_note`, `add_note_to_base`, `backlinks`.
- **Four of the six are read-only; two are writers.** `write_note` writes a note
  back to the vault and `add_note_to_base` creates one. Both go through the
  reconciler and the read-verify-write hash check, which protects the
  *content* — they do **not** decide *whether this caller may write*. Nothing in
  the tool layer authenticates the caller or checks a permission before a write
  reaches the backend.
- The transport is **unauthenticated**. A client that can open the socket is a
  client. There is no bearer token, no session identity, and no per-caller
  policy.
- Vault backends: filesystem and WebDAV (`src/vault/`). The WebDAV backend is
  already HTTP to a *storage* endpoint; that is a different trust boundary from
  this one — it authenticates outbound to a vault, and does nothing for inbound
  clients of the MCP server.

## Current assumption

The network is **assumed safe** for now: a private VPC / local network, no public
exposure, and no expectation that an untrusted party can reach the port. Under
that assumption the transport needs no authentication.

This is an assumption, not a control. It is worth being precise about what it
buys and what it does not:

- It buys *nothing* against anything that can already reach the port: another
  tenant in the VPC, a compromised sidecar, a container escape, a misconfigured
  security group, a developer laptop on the same network.
- It is the reason **nothing** below is scheduled. Every item on this list is
  optional exactly as long as the assumption holds, and load-bearing the moment
  it stops holding.

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
- Do **not** accept a credential in the URL query string or log it.
  `src/config.rs` already refuses userinfo in a URL and redacts URLs it reports
  (`redact_url`, `src/config.rs:251`) for exactly this reason — the same rule
  applies to a bearer token.

### 2. Authorization — which clients can call which tools

- Today: any client that can reach the socket can call any of the six tools,
  including both writers.
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

- Today: the HTTP transport is **plaintext**. There is no TLS termination in the
  server.
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
  expression from a stack overflow into a readable refusal. The parser's measured
  edge is **471** levels (`src/depth.rs:25`).
- Needed, once the surface is remote:
  - Treat the wire as untrusted. The depth limit is a resource guard, not an
    input-validation contract; it does not bound a huge *flat* payload, a giant
    note body, or a result set.
  - Bound request size and result size. A single `resolve_base` over a large
    vault can produce a very large payload, and an unauthenticated caller can
    request it repeatedly.
  - Keep the depth guard wired to the remote path. The guard must be reachable
    from an HTTP request, not only from the stdio path.
  - Re-check every path the write tools accept (`assert_portable_path`,
    `assert_note_path` in `src/drafts.rs:437` and `src/drafts.rs:398`). Those
    rules exist for portability; a remote caller can send a path a local caller
    never would.

### 6. Logging and audit trail

- Today: the only diagnostics are startup warnings on stderr
  (`src/main.rs:49`, `src/main.rs:61`, `src/main.rs:78` `warn`), and the stdio
  transport owns stdout for the protocol stream. There is no record of which
  caller invoked which tool.
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
  misconfigured deployment does not listen on every interface. **This is already
  the default** (`MCP_HTTP_HOST` defaults to `127.0.0.1`); the containerised
  deployment overrides it deliberately.
- Document the intended network boundary in the deployment docs, so the
  "private VPC" assumption above is stated where the operator reads it rather
  than only here. `docs/tooling.md` now documents the listener behaviour and the
  ownership rules a mounted vault needs.
- Keep the WebDAV backend's own credentials out of the MCP transport's trust
  model: the two are separate, and conflating them is how a storage credential
  ends up in an HTTP header.
- The container runs as a non-root user (uid 1000, pinned in the `Containerfile`)
  with no home and no shell. Under rootless podman the vault must be mounted
  with `--userns=keep-id` or the server cannot write it; guarden renders that
  itself from the image's probed user (ADR-0019 in the guarden repository). This
  is a *containment* control, not an access control: it bounds what an escaped
  container can reach, and says nothing about who may call the tools.

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
- `docs/tooling.md` — the gate that must stay green while this work lands, and
  the deployment instructions the operator reads.

## Out of scope (for this ticket)

- Vault content encryption at rest — a storage concern, not a transport one.
- Authorization *inside* a vault (per-note permissions). The tool surface serves
  one vault per process, so there is no per-note ACL to express yet; if that
  changes, it is a separate ticket.
- The WebDAV backend's outbound credential handling — already implemented, and
  covered by its own tests.
