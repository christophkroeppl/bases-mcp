# Task runner for this crate. `cargo` alone does not cover the things that are not
# cargo: the Obsidian CLI, the parity corpus, and the release flow.
#
# Recipes are grouped so `just --list` reads as the workflow rather than as a
# flat list of verbs.

set shell := ["bash", "-uc"]

# Show every recipe.
default:
    @just --list

# -- gates -------------------------------------------------------------------

# Format every source file in place.
fmt:
    cargo fmt

# Lint without failing the build, for a quick look.
lint:
    cargo clippy --all-targets

# What CI runs. Fast, no Obsidian, no network.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all-targets -- --skip parity
    @just check-versions

# -- tests -------------------------------------------------------------------

# Unit and integration tests, no Obsidian required.
test *ARGS:
    cargo test --all-targets -- --skip parity {{ARGS}}

# Everything, single-threaded, so a failing assertion is readable.
verify:
    cargo test --all-targets -- --skip parity --test-threads=1

# The Obsidian CLI gate. Panics by design if Obsidian cannot answer.
parity:
    cargo test --test parity

# Run everything, requiring the live Obsidian CLI.
check-all:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all-targets -- --test-threads=1
    @just check-versions

# -- build -------------------------------------------------------------------

build:
    cargo build --release

# -- Obsidian CLI ------------------------------------------------------------

# Is the CLI present, and which version?
cli-version:
    @command -v obsidian >/dev/null || { echo "obsidian CLI not on PATH"; exit 1; }
    @obsidian version

# Is the testing vault registered with Obsidian?
cli-vaults:
    @obsidian vaults

# Record the CLI's answers for every Base and view. Needs Obsidian.
record *ARGS:
    cargo run --release --bin parity-record -- {{ARGS}}

# Compare against the recorded snapshot, using the real CLI.
parity-recorded:
    cargo test --test parity

# Does the testing vault still match the recorded snapshot?
corpus-check:
    cargo run --release --bin parity-record -- --check

# -- housekeeping ------------------------------------------------------------

# Cargo.toml and Cargo.lock must agree with each other.
check-versions:
    @./scripts/check-versions.sh

# Pre-commit checks the commit is allowed to exist at all.
precommit:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all-targets -- --skip parity
    @just corpus-check

# What a new machine needs, once. Prints the path to register.
dev-setup:
    @./scripts/dev-setup.sh
