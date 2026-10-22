# bases-mcp

An MCP server that resolves Obsidian `.base` files to human- and agent-readable markdown.

One process serves **exactly one vault**. It exposes six tools, speaks either the
stdio transport (the default — one client, spawned by an MCP client) or Streamable
HTTP with SSE responses (many clients), and it keeps an audit trail of every tool
call.

The behaviour is pinned against the local Obsidian CLI. Where this server and
Obsidian differ on purpose, the difference is recorded in
[`docs/divergences.md`](docs/divergences.md) — a deliberate difference with
evidence, never a bug.

## Requirements

Rust **1.99 or newer** — the version `Cargo.toml` declares in `rust-version`.
The build image is pinned to `rust:1.99-bookworm` and CI to `1.99.1`, so the
container, CI and a developer machine build the same thing.

## Build

```bash
just build            # cargo build --release
# or
cargo build --release
```

The binary lands at `target/release/bases-mcp`. An MCP client has to be able to
execute that path.

## Run

```bash
bases-mcp --transport stdio      # one client over stdin/stdout (the default)
bases-mcp --transport http       # any number of clients over Streamable HTTP (SSE)
MCP_TRANSPORT=stdio|http         # the same choice, without a flag
```

`--transport` takes nothing else, and neither does `MCP_TRANSPORT`: a value this
server does not know is a refusal naming the value, not a silent default.
`bases-mcp --help` prints the whole surface.

The vault is always chosen by the environment, never by a flag. The server
refuses to start without one, because a server that quietly falls back to some
other vault answers questions from it and the client has no way to tell.

### With an MCP client

```json
{
  "mcpServers": {
    "bases-mcp": {
      "command": "/absolute/path/to/target/release/bases-mcp",
      "args": ["--transport", "stdio"],
      "env": {
        "BASES_MCP_VAULT": "/absolute/path/to/your/obsidian/vault"
      }
    }
  }
}
```

### The six tools

| Tool | What it does |
|---|---|
| `list_bases` | Lists every `.base` file in the vault with its views and layout types. |
| `resolve_base` | Resolves one view of a base into rows — `format=markdown` is the flat, CLI-parity surface; `format=json` is the structured one. |
| `get_note` | Reads a note as a **Projection**: every base region is replaced by a `base-rendered` fence carrying its provenance. |
| `write_note` | Applies an edit to a note. Base regions are matched by their path, not by position. |
| `add_note_to_base` | Adds a row to a base by creating a note its filter actually matches — a two-call handshake. |
| `backlinks` | Inbound links to a note, from the same resolver that backs every base. |

The last two write. The first four do not.

## Configuration

Everything is an environment variable. The table is exhaustive: no variable is
read that is not here, and no variable is here that is not read.

| Variable | Default | Notes |
|---|---|---|
| `BASES_MCP_VAULT` | — | Path to a vault directory. **Required** unless the WebDAV variables are used. Relative paths resolve against the **process working directory**; `~` is **not** expanded. |
| `BASES_MCP_WEBDAV_URL` | — | Base URL of the WebDAV collection. `http(s)` only, no credentials in it. Wins over nothing, loses to `BASES_MCP_VAULT`. |
| `BASES_MCP_WEBDAV_USER` | — | Basic-auth username. Required together with the password when the URL is set. |
| `BASES_MCP_WEBDAV_PASSWORD` | — | Basic-auth password. Never logged, never echoed in an error. |
| `MCP_TRANSPORT` | `stdio` | `stdio` or `http`. |
| `MCP_HTTP_HOST` | `127.0.0.1` | Bind address for the HTTP transport. Also decides which `Host` headers are served. |
| `MCP_HTTP_PORT` | `8765` | Bind port. `0` asks the kernel for a free port; the server announces the port it got. |
| `MCP_AUDIT_LOG` | stderr | File the audit trail is written to. Unset means stderr, and nothing rotates stderr. |
| `MCP_AUDIT_LOG_MAX_MB` | `5` | Rollover threshold, in **megabytes**. One previous generation is kept, so the disk cost is twice this. |

### Rules worth knowing before you hit them

- **Exactly one backend.** `BASES_MCP_VAULT` wins if both are set, and says so on
  stderr. Setting a WebDAV variable without `BASES_MCP_WEBDAV_URL` is a refusal.
- **A blank is not always "unset".** `MCP_HTTP_HOST=` is the default (loopback).
  `MCP_TRANSPORT=`, `MCP_HTTP_PORT=`, `MCP_AUDIT_LOG=`, `MCP_AUDIT_LOG_MAX_MB=`
  and `BASES_MCP_WEBDAV_URL=` are all **refusals** when set but blank. A blank
  `BASES_MCP_VAULT=` is a refusal too.
- **A WebDAV URL carries no credentials.** A URL with userinfo is refused, and
  the credentials are redacted in the message that says so. A query string or a
  fragment is dropped; a missing trailing slash is added.
- **The audit trail file is created 0600.** The previous generation is kept at
  `<path>.1`.
- **`MCP_HTTP_HOST` decides which `Host` headers are served.** A specific
  address is served on that address and on `localhost`; `0.0.0.0` serves
  **every** `Host` header, which makes the listener network-accessible.

The endpoint is `http://HOST:PORT/mcp`.

## Container

```bash
podman build -f Containerfile -t localhost/bases-mcp:latest .

podman run --rm --userns=keep-id \
  -v /path/to/vault:/vault:rw \
  -e BASES_MCP_VAULT=/vault \
  -e MCP_HTTP_HOST=0.0.0.0 \
  -p 127.0.0.1:8765:8765 \
  localhost/bases-mcp:latest --transport http
```

`MCP_HTTP_HOST=0.0.0.0` is required inside a container: the default
`127.0.0.1` binds the loopback *of the container*, and nothing outside it could
reach the server. The published port is bound to `127.0.0.1` on the host, so the
listener is reachable from the machine and not from the network.

**`--userns=keep-id` is not optional.** It is the difference between a container
that can modify the vault and one that only reads it. Without it every write
fails with `Permission denied (os error 13)` — which names the file and the
syscall and says nothing about the uid, so it reads like an immutable file.

`compose.yaml` is **not** a standalone compose file: it is the unit source for
`.guarden/service.yml`, which is why it carries no `build:`, no `ports:` and no
vault variable. A plain `docker compose up` against it will not work.

### Behind a reverse proxy

The listener speaks **plaintext HTTP** and has no TLS of its own. TLS is
terminated by whatever sits in front of it. `docs/reverse-proxy.md` has
copy-pasteable compose projects for Traefik, Caddy and nginx, each wired to the
`Host` rule above, the upstream path `/mcp`, and a non-buffered SSE response.

## Security

**The HTTP transport has no authentication, no authorization and no rate
limiting. Anyone who can open the socket can call every tool, including the two
that write.** The server does not enforce a network boundary; it *assumes* one.
That assumption is the deployment's job.

The boundary is **where the port is published**, not where the server binds. The
container binds every interface of the container — it has to, or nothing outside
it could dial it — and is still not on the network, because the host publishes it
to `127.0.0.1`.

What is built instead of a control is a record: every call, and every session's
opening, is written to the audit trail. It makes an incident answerable after
the fact and prevents nothing. See `docs/tooling.md` for the trail's shape and
`docs/divergences.md` D9 for the full trust model.

**Re-open the hardening list before this server is exposed beyond a network it
is trusted on.** Authentication, authorization, rate limiting and TLS are owed.

## Development

```
just check           fmt, clippy, tests without Obsidian. What CI runs.
just check-all       everything, requiring the live Obsidian CLI.
just parity          the Obsidian CLI gate on its own.
just dev-setup       what a new machine needs, once.
just --list          everything else.
```

Parity is a Cargo feature, not a `--skip` filter: `tests/parity.rs` is declared
with `required-features = ["parity"]`, so plain `cargo test` cannot build the
target at all.

## Licence

Apache License 2.0 — see [`LICENSE`](LICENSE). The SPDX identifier in
`Cargo.toml` is `Apache-2.0`; the two agree.

## Documentation

| | |
|---|---|
| [`docs/tooling.md`](docs/tooling.md) | Toolchain, transports, vault backends, the audit trail, the container, the Obsidian CLI gates. |
| [`docs/reverse-proxy.md`](docs/reverse-proxy.md) | TLS termination in front of the listener, the `Host` rule, three proxy examples. |
| [`docs/divergences.md`](docs/divergences.md) | Every deliberate difference from Obsidian, with evidence. |
| [`CONTEXT.md`](CONTEXT.md) | The domain language: Base, View, Host note, Projection, Property ID, Link value. |
| [`.env.schema`](.env.schema) | The environment variables, as a varlock schema. |
