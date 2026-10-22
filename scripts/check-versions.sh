#!/usr/bin/env bash
# Cargo.toml and Cargo.lock must agree with each other.
#
# `cargo build` would catch the drift, but only on a crate whose dependency graph
# actually changed, and it reports it as a lockfile problem rather than the
# version mismatch it is. It is also the check that runs on every commit, so it
# has to be fast and dependency-free.
set -euo pipefail

read_manifest_version() {
	awk '
        /^\[package\]/ { in_package = 1; next }
        /^\[/          { in_package = 0 }
        in_package && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' Cargo.toml
}

read_lock_version() {
	awk '
        /^name = "bases-mcp"$/ { found = 1; next }
        found && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' Cargo.lock
}

manifest_version="$(read_manifest_version)"
lock_version="$(read_lock_version)"

if [[ -z "$manifest_version" || -z "$lock_version" ]]; then
	echo "check-versions: could not read a version from Cargo.toml or Cargo.lock" >&2
	exit 1
fi

if [[ "$manifest_version" != "$lock_version" ]]; then
	echo "check-versions: Cargo.toml says $manifest_version but Cargo.lock says $lock_version" >&2
	echo "check-versions: fix with: cargo update -p bases-mcp" >&2
	exit 1
fi

echo "check-versions: agree at $manifest_version"
