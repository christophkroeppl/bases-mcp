#!/usr/bin/env bash
# The container cells: build the image from this tree, then drive it.
#
# FAILS when the image cannot be built or no OCI runtime can start a container.
# A "full" run that skipped the container cells would report a green suite that
# tested nothing, which is the failure this tier exists to prevent.
#
# Two probes, deliberately, because they answer different questions and only the
# second one is about this machine:
#
#   build   proves the image is well formed. It does NOT prove the runtime: a
#           fully cached build executes no RUN step and starts no container, so
#           it succeeds even on a host whose crun cannot start anything.
#   run     proves a container starts. `podman info` is not enough either -- it
#           answers happily with a crun that is too old, and the failure only
#           appears when a container actually starts.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

image="${BASES_MCP_CONTAINER_IMAGE:-localhost/bases-mcp:full}"
runc=/usr/bin/runc

die() {
	echo "container cells: $*" >&2
	exit 1
}

command -v podman >/dev/null || die "podman is not on PATH"

podman_cmd=(podman)
runtime="${BASES_MCP_CONTAINER_RUNTIME:-}"
[[ -n "$runtime" ]] && podman_cmd+=(--runtime "$runtime")

build() { "$@" build -f Containerfile -t "$image" . >&2; }
starts() { "$@" run --rm --entrypoint /bin/true "$image" >/dev/null 2>&1; }
run_cells() {
	BASES_MCP_CONTAINER_IMAGE="$image" \
	BASES_MCP_CONTAINER_RUNTIME="$1" \
		cargo test --test container_matrix -- --ignored --test-threads=1
}

echo "container cells: building $image" >&2
if ! build "${podman_cmd[@]}"; then
	[[ -n "$runtime" ]] && die "the image did not build with $runtime"
	[[ -x "$runc" ]] || die "the image did not build"
	build podman --runtime "$runc" || die "the image did not build with the default runtime or $runc"
	podman_cmd=(podman --runtime "$runc")
	runtime="$runc"
fi

if ! starts "${podman_cmd[@]}"; then
	[[ -n "$runtime" ]] && die "no container starts from $image with $runtime"
	[[ -x "$runc" ]] || die "the image is built, but no container starts from it and $runc is absent"
	starts podman --runtime "$runc" \
		|| die "the image is built, but no container starts from it with the default runtime or $runc"
	podman_cmd=(podman --runtime "$runc")
	runtime="$runc"
fi

[[ -n "$runtime" ]] && echo "container cells: the default OCI runtime cannot start a container; using $runtime" >&2
run_cells "$runtime"
