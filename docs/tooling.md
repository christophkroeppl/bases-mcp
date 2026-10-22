# Tooling

Rust crate, one toolchain. Every gate runs from `just`.

```
just check           fmt, clippy, tests without Obsidian. What CI runs.
just check-all       everything, requiring the live Obsidian CLI.
just parity          the Obsidian CLI gate on its own.
just dev-setup       what a new machine needs, once.
just --list          everything else.
```

| | Tool | Gate |
|---|---|---|
| Format | rustfmt | `just fmt` |
| Lint | clippy | `just lint` |
| Types | rust-analyzer / `cargo check` | `rust-analyzer diagnostics .` |

## Running the server

One binary, two transports. The vault is always chosen by the environment,
never by a flag.

```
bases-mcp --transport stdio   one client over stdin/stdout (the default)
bases-mcp --transport http    any number of clients over Streamable HTTP (SSE)
MCP_TRANSPORT=stdio|http      the same choice, without a flag
```

`--transport` takes nothing else, and neither does `MCP_TRANSPORT`: a value
this server does not know is a refusal naming the value, not a silent default.
`bases-mcp --help` prints the same surface.

### The two axes, and all four of their combinations

The transport decides **how bytes travel**. The backend decides **where the
vault lives**. They are independent, and every combination works:

| | filesystem vault | WebDAV vault |
|---|---|---|
| **stdio** | agent spawns the binary, talks over the pipes | the same, against a remote vault |
| **HTTP** | many clients over Streamable HTTP | many clients, against a remote vault |

`tests/cross_product.rs` drives all four cells end to end — it spawns the real
binary, speaks MCP to it the way a client does (`initialize`, `tools/list`,
`tools/call`), and checks each write **at the far end**: the file on disk, or
the bytes the WebDAV server actually stored. A write that reports success
without landing anywhere fails that suite, which is the failure a read-only
matrix cannot see.

The other axis is **deployment**: the same binary, native or in the image.
`tests/container_matrix.rs` covers the four container cells the same way — the
real image, the real binary inside it, the real transports, and a far-end
check on every write. It needs a built image, so it is opt-in:

```bash
podman --runtime /path/to/crun build -f Containerfile -t localhost/bases-mcp:latest .
BASES_MCP_CONTAINER_IMAGE=localhost/bases-mcp:latest \
BASES_MCP_CONTAINER_RUNTIME=/path/to/crun \
  cargo test --test container_matrix -- --test-threads=1
```

Without `BASES_MCP_CONTAINER_IMAGE` the suite **skips**, and it reports the
skip as a skip — never as a pass. `BASES_MCP_CONTAINER_RUNTIME` names the OCI
runtime for hosts whose system `crun` predates their kernel.

Two things that suite needs and that a hand-run does not: the container
reaches a host-side WebDAV server through `--add-host
host.containers.internal:host-gateway` (inside a container, `127.0.0.1` is the
container's own loopback), and it sets **either** `BASES_MCP_VAULT` **or**
`BASES_MCP_WEBDAV_URL` but never both, because `BASES_MCP_VAULT` wins over
the WebDAV variables and a container with both is a filesystem server
pointed at an empty mount.

### The vault

Exactly one backend, and it must be named:

```
BASES_MCP_VAULT              path to a vault directory on the local filesystem
BASES_MCP_WEBDAV_URL         WebDAV base URL, no credentials in it
BASES_MCP_WEBDAV_USER        WebDAV username
BASES_MCP_WEBDAV_PASSWORD    WebDAV password
```

`BASES_MCP_VAULT` wins if both are set, and says so on stderr. A selector that
is set but blank is an error rather than an absence. All five live in
`.env.example`, which is the documented home for environment variables.

### The HTTP endpoint

The listener is configured by two variables, and both refuse a value they
cannot use:

```
MCP_HTTP_HOST    bind address (default 127.0.0.1)
MCP_HTTP_PORT    bind port (default 8765)
```

The endpoint is `http://HOST:PORT/mcp`, and it is Streamable HTTP with SSE
responses: `POST` carries a request, `GET` opens the SSE stream, `DELETE` closes
a session. Each `initialize` returns an `Mcp-Session-Id` the client echoes on
every later request.

`allowed_hosts` is derived from the bind address, because rmcp rejects a `Host`
header it does not recognise with a bare `403 Forbidden` and has no way to know
which address a connection arrived on:

| Bind | Served `Host` |
|---|---|
| `127.0.0.1` (default) | `localhost`, `127.0.0.1` |
| a specific address | that address, plus `localhost` |
| `::1` | `localhost`, `::1` |
| `0.0.0.0` / `::` | **any** — every interface, so no address is knowable |

The list is the **loopback of the bound family only**. A `127.0.0.1` listener
serves `127.0.0.1`, not `::1`: a client on the other stack could not have
reached it, so granting that loopback would grant nothing. Measured against
the built binary with `MCP_HTTP_HOST=127.0.0.1`: `Host: 127.0.0.1` and
`Host: localhost` answer `200`; `Host: [::1]` and `Host: example.com` answer
`403`. `src/transport.rs` carries the unit test that pins this.

Binding `0.0.0.0` therefore opens the server to every `Host` header, which is
the operator's explicit choice, is printed on stderr at startup, and is recorded
as a Divergence in `docs/divergences.md`. It is not a default anyone has to undo,
and it is not safe on a network this server is not trusted on: the listener has
no authentication. That gap is recorded, not hidden.

#### The network boundary, stated where the operator reads it

**The HTTP transport has no authentication, no authorization and no rate
limiting. Anyone who can open the socket can call every tool, including the
two that write.** The server does not enforce a network boundary; it *assumes*
one. That assumption is the deployment's job, and this is where it is written
down.

The three deployments this project ships, and what each one assumes:

| Deployment | Who can reach the listener | The assumption |
|---|---|---|
| **stdio** (the default) | the process that spawned it | none — the pipes are the boundary |
| **HTTP on loopback** | processes on this machine | a local machine is a trusted one |
| **HTTP in a container** | whatever the container's port is published to | the publication is a loopback one |

So the boundary is **where the port is published**, not where the server binds.
The container binds every interface of the container — it has to, or nothing
outside it could dial it — and is still not on the network, because the host
publishes it to `127.0.0.1`. A deployment that publishes the port to
`0.0.0.0` on the host has put an unauthenticated read/write server on the
network, and nothing in this repository stops it or notices it.

Two of the six tools write: `write_note` and `add_note_to_base`. They protect
the note's *content* — the reconciler and the read-verify-write hash check —
and they do **not** decide *whether this caller may write*.

What would close the gap is authentication on the transport, a policy for which
client may call which tool, rate limiting, and TLS. None of that is built; the
hardening list is at the repository root in `BACKLOG-network-security.md`, and
`docs/divergences.md` D9 records the gap against this surface. **Re-open that
list before this server is exposed beyond a network it is trusted on.**

#### The container's non-root user is a containment control, not an access control

The image runs as uid 1000 with no home and no shell, and the vault is the
only thing it can write to. That bounds what an escaped container can reach.

It says **nothing about who may call the tools.** A non-root process serving an
unauthenticated HTTP listener is still an unauthenticated HTTP listener: the
uid is the server's own privilege, not a credential the caller presents. The
two are different boundaries and only one of them is closed.

`--userns=keep-id` is a third thing again: it is about *ownership*, so the
server can write a vault whose files belong to the invoking user. It changes
whose files the server may modify, not who may talk to it.

### The transport is tested over the wire

`tests/http_transport.rs` and `tests/stdio_transport.rs` spawn the real binary
and drive it as an MCP client. The HTTP suite asserts the handshake, the six
tools and their schemas, a tool call that reaches the engine, a concurrent run
across six sessions, and that a `Host` naming the bound address is served while
a neighbour is refused. The stdio suite drives `initialize`, `tools/list` and
`tools/call` through stdin/stdout, and asserts `MCP_TRANSPORT` selects the
transport it names.

## The container

One image serves both transports. The transport is chosen by the **command**,
not by the image, so the same image works for a deployment that needs a
listener and for an agent that spawns the binary.

```
podman build -f Containerfile -t localhost/bases-mcp:latest .
```

If that fails with `OCI runtime error: crun: unknown version specified`, the
host's default OCI runtime is too old for its kernel and a newer one is
installed elsewhere:

```
podman --runtime /path/to/crun build -f Containerfile -t localhost/bases-mcp:latest .
```

That is a property of the HOST, not of this image: the failure names the
runtime, and it happens before the `Containerfile` is read at all.

### The image is pinned to the toolchain the LOCKFILE needs

The crate declares `rust-version = "1.85"` — the edition-2024 floor — but that
is not the constraint the build has. The **locked dependency graph** is:
`rmcp 3.5.0` and the `icu` crates require rustc 1.88, and `cargo build
--locked` refuses the whole graph on an older compiler. The build image is
pinned to `rust:1.88-bookworm` for that reason — an older image fails with
"rmcp@3.5.0 requires rustc 1.88", which names a dependency rather than the
image.

The image also carries a placeholder at `tests/parity.rs`. `Cargo.toml`
declares that test target by path, and a declared target whose file is missing
is a **manifest error**: cargo fails to parse the manifest, before compiling
anything. The suite itself is excluded from the build context (it needs a live
Obsidian CLI), so the placeholder satisfies the manifest and nothing else.

### Running it against a local vault

```bash
podman run --rm \
  -v /path/to/vault:/vault:rw \
  -e BASES_MCP_VAULT=/vault \
  -e MCP_HTTP_HOST=0.0.0.0 \
  -p 127.0.0.1:8765:8765 \
  localhost/bases-mcp:latest --transport http
```

`MCP_HTTP_HOST=0.0.0.0` is required inside a container: the default
`127.0.0.1` would bind the loopback *of the container*, and nothing outside
it could reach the server. The published port is bound to `127.0.0.1` on the
host, so the listener is reachable from the machine and not from the network.

The image runs as a non-root user with no home and no shell, and the vault is
the only thing it can write to.

#### The vault must be writable, and `--userns=keep-id` is how

**`--userns=keep-id` is not optional.** It is the difference between a
container that can modify the vault and one that only reads it.

Under rootless podman (the default), the invoking user's **host** uid is
mapped to **container root**. So a host file owned by uid 1000 appears inside
the container as owned by uid 0, and the server — which runs as uid 1000 — is
not its owner and cannot write it. The file's mode bits are unchanged; only the
identity behind them moved. `--userns=keep-id` maps the host uid to the *same*
uid inside, so the server owns the notes it is asked to modify.

```bash
podman run --rm --userns=keep-id \
  -v /path/to/vault:/vault:rw \
  -e BASES_MCP_VAULT=/vault \
  -e MCP_HTTP_HOST=0.0.0.0 \
  -p 127.0.0.1:8765:8765 \
  localhost/bases-mcp:latest --transport http
```

The image's user is **uid 1000** (`bases-mcp`), pinned in the `Containerfile`
rather than left to `useradd --system`, which picks 999 on Debian. With
`--userns=keep-id` that is the host user's own uid, so the vault stays owned
by the person who owns it — no `chown`, no group, no ACL.

**The failure without it is quiet, and it is worth knowing what it looks
like.** Measured on this host, a vault the container can read but not write
**reads normally** — every Base comes back with its rows, because the notes
are world-readable — and fails every WRITE with `Permission denied (os error
13)`. That error names the file and the syscall, and says nothing about the
uid, so the obvious reading is that the file is immutable or the mount is
read-only. It is neither: the mode bits are 644, the mount is `rw`, and the
server simply is not the owner.

A vault the container cannot READ at all is indexed as an EMPTY vault, so
every Base comes back with zero rows and every note reads as "not found" —
which looks exactly like an empty vault rather than like a permissions
problem. Neither shape says "the uid is wrong".

Both shapes are covered by `tests/container_matrix.rs`, which asserts the
read that must succeed and the write that must fail.

#### A file the container CREATES is yours — including the group

`--userns=keep-id` maps the invoking user to the same uid, and a file the
container **creates** is owned by that uid: `uid=1000` on this host, your own.
Measured, not assumed:

```bash
podman run --rm --userns=keep-id \
  -v /path/to/vault:/vault -e BASES_MCP_VAULT=/vault \
  --entrypoint /bin/sh localhost/bases-mcp:latest -c 'touch /vault/created.md'
stat -c '%n uid=%u gid=%g mode=%a' /path/to/vault/created.md
#   /path/to/vault/created.md uid=1000 gid=100999 mode=644
```

The **owner** is your uid, so you can read, write, and delete the file, and a
process running as your ordinary uid can write it. The `100999` in the *group*
slot is a subuid, and it does not stand in your way: the mode bits are 644,
the owner's write bit is set, and the owner is you. **No `chown` is needed.**

The subuid lands in the group slot, not the owner slot, because `keep-id`
maps *your* uid to itself and falls back to the subuid range for the group.
This is a property of rootless containers generally, not of this image.

The group is worth noticing for one reason: a file the container creates is
`644` while a file you create in that directory may be `664`, and the
directory itself is `755`. Neither stops you. It only means the container's
files are not group-writable, so a *second* user who shares the group cannot
write them — which is the behaviour you would want anyway.

#### On a guarden host, the namespace is already handled

`compose.yaml` does **not** carry a user-namespace setting, and that is
deliberate: guarden refuses `userns_mode` in a Compose file because the
namespace is the host's business, not the repository's. Guarden probes the
**image's** user and renders `Userns=keep-id:uid=…,gid=…` into the unit
itself (ADR-0019) — mapping the image user onto the service identity, and
never chowning files.

Which is the other reason the image's uid is **pinned**: guarden can only pin
a uid it can read out of the image, and a repository that leaves the uid to
`useradd --system` gives the host a different number to pin on every base
image it rebuilds.

### Running it against a WebDAV vault

```bash
podman run --rm \
  -e BASES_MCP_WEBDAV_URL=https://dav.example/vault \
  -e BASES_MCP_WEBDAV_USER=... \
  -e BASES_MCP_WEBDAV_PASSWORD=... \
  -e MCP_HTTP_HOST=0.0.0.0 \
  -p 127.0.0.1:8765:8765 \
  localhost/bases-mcp:latest --transport http
```

No volume is needed — the vault is remote. The credentials are read from the
environment and are never logged or echoed in an error.

### Serving over stdio instead

```bash
podman run --rm -i --userns=keep-id \
  -v /path/to/vault:/vault:rw \
  -e BASES_MCP_VAULT=/vault \
  localhost/bases-mcp:latest --transport stdio
```

`-i` keeps stdin open; without it the container's stdin is closed and the
protocol stream ends immediately.

## Obsidian CLI gates

`just check` does not need Obsidian. `just parity`, `just check-all` and
`just record` do. `just corpus-check` and `just parity-recorded` do not: they
read the recorded corpus, not the CLI.

Parity is a **Cargo feature**, not a `--skip` filter:

```
$ cargo test --features parity --test parity
```

`tests/parity.rs` is declared with `required-features = ["parity"]`, so plain
`cargo test` cannot build the target at all. That is deliberate. `--skip parity`
filters by test *name*, and only one of the eight tests in that file has "parity"
in its name — the other seven ran, three of them panicked at the no-CLI guard, and
CI was red on every push while its own comment claimed parity was excluded. The
same filter was also quietly dropping `render_is_the_flat_cli_parity_surface` and
`resolve_base_says_which_of_its_two_surfaces_is_the_parity_one` from suites that
never touch the CLI. Filtering by name is not a substitute for excluding a target.

`tests/parity.rs` **panics** when the CLI cannot answer rather than skipping, so
a green suite always compared something. `BASES_MCP_ALLOW_SKIP=1` relaxes it, and
that is a deliberate choice: the alternative is a parity suite that passes
because it tested nothing, which is the failure this project has already made
once. Because that panic is the guard, `just check` runs the suite with
`OBSIDIAN_BIN` pointed at a path that cannot exist — so the gate that excludes
parity is *proved* on every run rather than assumed, and CI runs the same
assertion as a step of its own.

`just dev-setup` prints whether the CLI is on `PATH`, its version, the registered
vaults, and — if the testing vault is missing — the exact path to add. The CLI
cannot register a vault itself, so that last step is manual by necessity rather
than by omission.

## opencode.jsonc

Project-scoped, plain JSON, no comments.

```
formatter  rustfmt   rustfmt --edition 2024 $FILE     .rs
formatter  prettier  disabled
lsp        rust      rust-analyzer                   .rs
```

`--edition 2024` here is the **style** edition, and it matches both `Cargo.toml`
and `rustfmt.toml`: edition 2024's style sorts imports version-wise, so the
edition bump reordered `use` blocks across the tree. A bare `rustfmt file.rs`
uses this flag; `cargo fmt` passes the manifest's edition itself.

There is **no `linter` key** in opencode's config schema; it sets
`additionalProperties: false`, so writing one is a validation error. Linting is
wired in as a language server, which is also what opencode's own guidance
recommends.

### Three things that validated and then did nothing

Each passed a schema check and was still wrong. Recorded because the schema is
not the specification.

1. **An empty LSP entry is not "no LSP".** `{"rust": {}}` is permitted by the
   schema and rejected by opencode at load: `Missing key lsp.rust.command`. An
   entry needs an explicit command.
2. **The Rust LSP id is `rust`, not `rust-analyzer`.** The class is called
   `RustAnalyzer`; the id is `rust`. The wrong name validated and attached
   nothing.
3. **An empty diagnostics result is not "no problems".**
   `opencode debug lsp diagnostics` disposes the server about 100 ms after
   touching the file, and rust-analyzer needs roughly 45 s to index this crate.
   Every `.rs` result is `[]` whether or not the code compiles.

So the honest check for Rust types is `rust-analyzer diagnostics .`, plus
`cargo check` in CI. Do not treat `opencode debug lsp diagnostics` as evidence
about a `.rs` file.

## Verifying a change

`opencode debug config` shows what opencode actually resolved, including entries
inherited from a global config. To prove a formatter fires, write an unformatted
file and confirm it is reformatted on disk. To prove an LSP is attached,
introduce a real type error and confirm it is reported — `[]` reads as clean
whether or not the server is working.

## Adding a test file

A new file under `tests/` is its own crate, and `tests/common/mod.rs` is
compiled into every crate that declares `mod common;`. Each of those crates
uses a different subset of the shared helpers, so a helper used by one is
dead code in the others — and clippy runs with `-D warnings`.

That used to mean the global gate broke the moment a new file declared
`mod common;` and used only part of it: 33 dead-code errors, over helpers the
new file never touched. The `dead_code` allow now lives **inside**
`tests/common/mod.rs`, so it travels with the module into every crate that
includes it, and a new file can use whatever subset it likes:

```rust
mod common;

use common::vault_dir;   // the only helper this file needs
```

Two things the allow deliberately does NOT do:

- It does not reach the crate that *includes* the module. A suite's own dead
  function is still reported, because that is a fault in the suite.
- It does not hide a helper that **no** suite uses. That is real dead code,
  and `tests/common_subset.rs` fails the gate over it. The rule is blunt on
  purpose: a helper counts as used when a suite mentions it or a sibling in
  the module mentions it, and it under-reports rather than over-reports, so
  it never fails the gate over a helper that is alive.

So: a new test file may use any subset of the shared helpers, and a new
helper in `tests/common/` must be used by some suite.

## Corpus

`test/vault` is the Testing vault: the parity oracle, identical on every branch,
nine files, and read-only to every tool that is not explicitly a writer.
`test/fixtures` holds one Fixture vault per spec construct, each isolating one
construct and never registered in Obsidian.

`.obsidian/` inside `test/vault` is Obsidian's own state and is gitignored.

### The recorder

`src/bin/parity-record.rs` (over `src/parity.rs`) records what the CLI answers
for every view of every base, so a parity comparison can run where Obsidian
cannot. Three verbs, and only the first needs the app:

**The vault is pinned by a manifest, not only by a count.** `test/vault.manifest`
records every file of the oracle with its SHA-256, and the gate compares the
vault against it on every run:

```bash
cd test/vault && sha256sum -c ../vault.manifest        # GNU coreutils
cd test/vault && shasum -a 256 -c ../vault.manifest    # macOS
```

A count could only see a file being added or removed. A file whose bytes changed
is still one file, so the desktop app rewriting `Tickets.base` — which is what
running any `obsidian` command does — left the count at nine while the oracle had
silently changed what `obsidian base:query` returns. The manifest names the
offender, the digest it expected, and the digest it found. The format is
`sha256sum`'s own, so the stock tool checks it; the digest is over the file's
bytes, so no line-ending normalisation can call a rewrite a match.

The manifest changes only deliberately: `cargo test --test oracle_manifest --
--ignored regenerate` rewrites it, and the diff is the review.

```bash
just record          # record:  ask the LIVE CLI, store the answers
just corpus-check    # check:   validate the store — no CLI, no vault
just parity-recorded # compare: resolve the vault ourselves and compare with the store
```

`record` uses `vault=` plus the `path=`/`view=`/`format=json|md` spelling that
`tests/parity.rs` also uses, and records every view of every base in the vault.
`--vault`, `--vault-dir` and `--data-dir` override the defaults (`vault`,
`test/vault`, `test/parity/data`).

The store is `test/parity/data/<digest>/`, where the digest is a sha256 over
every recorded answer. JSON is canonicalised (object keys sorted) before it is
hashed, so two Obsidian versions that agree on values share one directory even
if they spell key order differently; a version that answers differently gets its
own. The CLI version goes in `manifest.json` for a human and is **not** the key.
Each entry is stored twice — `<base>/<view>.json` for the rows and
`<base>/<view>.md` for the rendering — and the manifest records a digest per
file, so `check` can name the file that rotted rather than "the corpus".

### Recording is the one step a script cannot do

The CLI is a client to the running app: it answers for the vault the app has
open, whichever checkout the command was issued from. `record` therefore reads
the registered path for `vault=<name>` from Obsidian's own `obsidian.json`
(`OBSIDIAN_CONFIG_DIR` overrides its location) and refuses unless that path is
the `--vault-dir` it is enumerating. Otherwise the corpus would pair one vault's
bases with another vault's answers and read as parity.

It also hashes the vault before and after the run. Obsidian normalises `.base`
YAML when it loads a vault, and `test/vault` is an oracle whose bytes are
asserted; if a byte moved, `record` fails, names the files, and writes nothing.
It cannot prevent the rewrite — only a human can decide to let Obsidian touch a
vault — but it refuses to launder one into a corpus.

### There is no recorded corpus yet

`test/parity/data/` does not exist yet. Recording needs a human in the loop: the
app must have *this checkout's* `test/vault` open, and opening a vault is a
desktop act, not a command. Until someone records, `just corpus-check` exits 1
and says so, and `just parity-recorded` has no corpus to compare against.
`just check` does **not** chain `corpus-check`: a gate that cannot pass is not a
gate. Once a corpus is committed, `corpus-check` and `parity-recorded` run in CI
without Obsidian — that is the point of recording one.
