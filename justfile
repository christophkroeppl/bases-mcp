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
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    OBSIDIAN_BIN=/nonexistent/obsidian cargo test --all-targets
    @just check-versions

dev-setup:
    @./scripts/dev-setup.sh
