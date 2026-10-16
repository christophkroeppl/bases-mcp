# bases-mcp: an MCP server that resolves Obsidian Bases to markdown.
#
# Multi-stage: the release binary is built against the crate's locked
# dependencies, then copied into a minimal runtime image. `cargo build` needs
# the whole crate (src/, Cargo.toml, Cargo.lock), so the build context is the
# service's home — and `.dockerignore` keeps the target directory and the
# rootless image store out of that context.
FROM docker.io/rust:1.85-bookworm AS build

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM docker.io/debian:bookworm-slim

# ca-certificates: the WebDAV backend speaks TLS, and rustls needs the system
# trust store (the crate is built with default-features = false).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /build/target/release/bases-mcp /usr/local/bin/bases-mcp

# The listener is unauthenticated, so the binary runs as a user with nothing to
# reach: no home, no shell, and no write access outside a vault it is given.
# A container that is escaped then escapes as nobody rather than as root.
RUN useradd --system --no-create-home --shell /usr/sbin/nologin bases-mcp
USER bases-mcp

# The transport is chosen by the command, not the image: the same binary serves
# stdio when an agent spawns it. The deployment's compose passes
# `--transport http`; this default keeps a bare `podman run` useful.
ENTRYPOINT ["/usr/local/bin/bases-mcp"]
