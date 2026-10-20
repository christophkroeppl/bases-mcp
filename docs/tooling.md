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
| `127.0.0.1` (default) | `localhost`, `127.0.0.1`, `::1` |
| a specific address | that address, plus `localhost` |
| `0.0.0.0` / `::` | **any** — every interface, so no address is knowable |

Binding `0.0.0.0` therefore opens the server to every `Host` header, which is
the operator's explicit choice, is printed on stderr at startup, and is recorded
as a Divergence in `docs/divergences.md`. It is not a default anyone has to undo,
and it is not safe on a network this server is not trusted on: the listener has
no authentication. That gap is recorded, not hidden.

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

### The image is pinned to the toolchain the LOCKFILE needs

The crate declares `rust-version = "1.75"`, but that is not the constraint the
build has. The **locked dependency graph** is: `rmcp 3.5.0` and the `icu`
crates require rustc 1.88, and `cargo build --locked` refuses the whole graph
on an older compiler. The build image is pinned to `rust:1.88-bookworm` for
that reason — an older image fails with "rmcp@3.5.0 requires rustc 1.88",
which names a dependency rather than the image.

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

#### The vault must be writable by uid 1000

The image's user is **uid 1000** (`bases-mcp`), pinned in the `Containerfile`
rather than left to `useradd --system`, which picks 999 on Debian. A pinned
uid is what makes the vault writable: the container has to be able to write the
notes it is asked to modify, and the only way an operator can arrange that is
to know which uid to arrange it for.

So the vault directory must be readable **and writable** by uid 1000:

```bash
sudo chown -R 1000:1000 /path/to/vault
# or, to keep your own ownership and just grant the group:
sudo chown -R :1000 /path/to/vault && sudo chmod -R g+rwX /path/to/vault
```

**The failure without this is quiet, and it is worth knowing what it looks
like.** A vault directory the container cannot read is indexed as an EMPTY
vault, so every Base comes back with zero rows and every note reads as "not
found" — which looks exactly like an empty vault rather than like a
permissions problem. And a vault it can read but not write answers reads
normally and fails every write with `Permission denied (os error 13)`. Neither
says "the uid is wrong".

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
podman run --rm -i \
  -v /path/to/vault:/vault:rw \
  -e BASES_MCP_VAULT=/vault \
  localhost/bases-mcp:latest --transport stdio
```

`-i` keeps stdin open; without it the container's stdin is closed and the
protocol stream ends immediately.

## Obsidian CLI gates

`just check` does not need Obsidian. `just parity` and `just check-all` do.

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
formatter  rustfmt   rustfmt --edition 2021 $FILE     .rs
formatter  prettier  disabled
lsp        rust      rust-analyzer                   .rs
```

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

## Corpus

`test/vault` is the Testing vault: the parity oracle, identical on every branch,
nine files, and read-only to every tool that is not explicitly a writer.
`test/fixtures` holds one Fixture vault per spec construct, each isolating one
construct and never registered in Obsidian.

`.obsidian/` inside `test/vault` is Obsidian's own state and is gitignored.

Recorded CLI answers live under `test/parity/data/`, keyed by a digest of the
results rather than by version, so several Obsidian versions that agree share one
directory. `just record` refreshes them; `just corpus-check` reports whether the
Testing vault still matches the recording.
