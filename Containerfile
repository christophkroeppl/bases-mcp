# bases-mcp: an MCP server that resolves Obsidian Bases to markdown.
#
# Multi-stage: the release binary is built against the crate's locked
# dependencies, then copied into a minimal runtime image. `cargo build` needs
# the whole crate (src/, Cargo.toml, Cargo.lock), so the build context is the
# service's home — and `.dockerignore` keeps the target directory and the
# rootless image store out of that context.
# The pinned toolchain has to satisfy the LOCKFILE, not just the crate's own
# `rust-version`. The crate says `rust-version = "1.85"` — the edition-2024
# floor — but the locked dependency graph needs more: `rmcp 3.5.0` and the
# `icu` crates require rustc 1.88, and `cargo build --locked` refuses the
# whole graph on an older compiler. `rust:1.85-bookworm` therefore fails with
# "rmcp@3.5.0 requires rustc 1.88" before a single crate of this project is
# compiled.
#
# 1.88 is the floor the lockfile sets, and the image is pinned to it so the
# container and CI build the same thing. A newer image also works; an older one
# does not, and the failure is a confusing one that names a dependency rather
# than the image.
FROM docker.io/rust:1.88-bookworm AS build

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# `Cargo.toml` declares `[[test]] name = "parity"` with a path of
# `tests/parity.rs`. Cargo parses the manifest BEFORE it builds anything, and a
# declared test target whose file is missing is a manifest error, not a warning:
# the build fails with "can't find `parity` test at `tests/parity.rs`" before a
# single crate is compiled. So the file has to be in the image even though the
# parity suite never runs there -- it needs a live Obsidian CLI and a
# registered vault, neither of which exists in a container.
#
# The suite itself is excluded from the build CONTEXT by `.dockerignore`
# (`test/`), so this is a placeholder rather than the real file: the parity
# suite needs an Obsidian CLI, and no container has one.
RUN mkdir -p tests && printf '// The parity suite needs a live Obsidian CLI and is not run in the container.\\n// This file exists so Cargo can parse the manifest that declares it.\\n' > tests/parity.rs
# Only the server binary. `src/bin/parity-record.rs` is a development tool that
# records the parity corpus from a live Obsidian CLI; nothing in this image can
# run it, so it is not linked here even though the source is in context.
RUN cargo build --release --locked --bin bases-mcp

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
#
# The uid is PINNED to 1000 rather than left to `useradd --system`, which picks
# 999 on Debian and can pick something else on another base. A pinned uid is
# what makes the vault writable -- but only under the right USER NAMESPACE,
# and that is the part worth reading.
#
# Under rootless podman (the default), the invoking user's host uid is mapped
# to CONTAINER ROOT. A host file owned by uid 1000 therefore appears inside the
# container as owned by uid 0, and this process -- uid 1000 -- is not the
# owner and cannot write it. The file's mode bits are unchanged; only the
# identity behind them moved.
#
# The fix is `--userns=keep-id`, which maps the host uid to the SAME uid
# inside the container: the host's uid 1000 is uid 1000 in here, so the
# process owns the files it has to modify. See docs/tooling.md.
RUN useradd --system --uid 1000 --no-create-home --shell /usr/sbin/nologin bases-mcp
USER bases-mcp

# The transport is chosen by the command, not the image: the same binary serves
# stdio when an agent spawns it. The deployment's compose passes
# `--transport http`; this default keeps a bare `podman run` useful.
ENTRYPOINT ["/usr/local/bin/bases-mcp"]
