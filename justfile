set shell := ["bash", "-uc"]

default:
    @just --list

# -- gates -------------------------------------------------------------------

fmt:
    cargo fmt

lint:
    cargo clippy --all-targets

check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    OBSIDIAN_BIN=/nonexistent/obsidian cargo test --all-targets
    @just check-versions

# -- tests -------------------------------------------------------------------

test *ARGS:
    cargo test --all-targets {{ARGS}}

verify:
    cargo test --all-targets -- --test-threads=1

parity:
    cargo test --features parity --test parity

check-all:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --features parity --all-targets -- --test-threads=1
    @just check-versions

# -- test tiers --------------------------------------------------------------

# What pre-commit and CI-on-push run: no image, no Obsidian CLI.
test-fast:
    just check

# The full tier: the image is built AND driven, and the CLI parity suite runs
# when Obsidian answers. Fails when the image cannot be built or no OCI runtime
# can start a container -- containers are a shipped feature, and a "full" run
# that skipped them would be a green suite that tested nothing.
#
# A set BASES_MCP_ALLOW_SKIP excludes parity instead, which is how CI-main says
# "no CLI here" without hiding a failure.
#
# What `wt merge` runs.
test-full:
    just check
    just parity-if-up
    just container

# The CLI parity suite, skipped LOUDLY when the CLI cannot answer.
#
# A real query is the probe: `obsidian version` answers with a dead bridge, and
# the exit code is 0 either way, so neither is a signal.
#
# Parity against the live CLI, or a stated skip.
parity-if-up:
    @if [[ -n "${BASES_MCP_ALLOW_SKIP:-}" ]] || ! obsidian base:query path=AllNotes.base format=json 2>/dev/null | grep -q '^\['; then \
        echo "parity: the CLI is not answering -- skipped deliberately"; \
        BASES_MCP_ALLOW_SKIP=1 cargo test --features parity --test parity -- --nocapture; \
    else \
        cargo test --features parity --test parity -- --nocapture; \
    fi

container:
    @./scripts/container-suite.sh

# -- build -------------------------------------------------------------------

build:
    cargo build --release

# -- transport probes --------------------------------------------------------

probe-transport:
    @./scripts/probe-transport.sh

probe-tool-bodies:
    @./scripts/probe-tool-bodies.sh

# -- Obsidian CLI ------------------------------------------------------------

cli-version:
    @command -v obsidian >/dev/null || { echo "obsidian CLI not on PATH"; exit 1; }
    @obsidian version

cli-vaults:
    @obsidian vaults

record *ARGS:
    cargo run --quiet --bin parity-record -- record {{ARGS}}

parity-recorded *ARGS:
    cargo run --quiet --bin parity-record -- compare {{ARGS}}

corpus-check *ARGS:
    cargo run --quiet --bin parity-record -- check {{ARGS}}

# -- housekeeping ------------------------------------------------------------

check-versions:
    @./scripts/check-versions.sh

# Dependency advisories. Needs `cargo install cargo-audit` once.
audit:
    cargo audit

precommit:
    just test-fast

dev-setup:
    @./scripts/dev-setup.sh
