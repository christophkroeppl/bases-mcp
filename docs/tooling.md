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
