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
    @echo "parity-record is not built yet." >&2
    @echo "This recipe needs a snapshot recorder that does not exist." >&2
    @exit 1

parity-recorded:
    cargo test --features parity --test parity

corpus-check:
    @echo "parity-record is not built yet." >&2
    @echo "There is no snapshot corpus to check against test/parity/data/." >&2
    @exit 1

# -- housekeeping ------------------------------------------------------------

check-versions:
    @./scripts/check-versions.sh

precommit:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    OBSIDIAN_BIN=/nonexistent/obsidian cargo test --all-targets
    @just check-versions

dev-setup:
    @./scripts/dev-setup.sh
